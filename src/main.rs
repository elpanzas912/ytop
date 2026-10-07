#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod audio;

slint::include_modules!();

use audio::Shared;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, TimerMode, VecModel, Weak};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Track {
    id: String,
    title: String,
    artist: String,
    dur: u64,
    channel_id: String,
    views: u64,
    date: String, // AAAAMMDD (si se conoce)
    music: bool,
    thumb_id: String, // id del video de cuya miniatura se dibuja (en listas: el primero)
    kind: u8,         // 0 video, 1 lista de reproduccion / mix
    count: u32,       // cantidad de videos de la lista (si se sabe)
}

/// De donde salio la lista actual (para "cargar mas").
#[derive(Clone)]
struct ViewSrc {
    tab: i32,
    source: String,
    n: usize,
    music: bool,
    query: Option<String>,
    lists: bool,
}

/// Lo que se veia antes de abrir una lista (para el boton Volver).
struct BackView {
    tab: i32,
    heading: String,
    tracks: Vec<Track>,
    src: Option<ViewSrc>,
}

#[derive(Default)]
struct State {
    view: Vec<Track>,
    queue: Vec<Track>,
    cur: Option<usize>,
    cache: HashMap<i32, Vec<Track>>,
    thumb_state: Vec<u8>, // 0 sin imagen, 1 en cola, 2 cargada
    src: Option<ViewSrc>,
    queue_title: String,
    back: Option<BackView>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}
/// Identifica cada peticion de datos (busqueda, pestaña, lista, cargar mas); solo vale el ultimo.
static VIEW_GEN: AtomicU64 = AtomicU64::new(0);
/// Identifica la tanda de miniaturas vigente (cambia con la vista o el tamaño de tarjeta).
static THUMB_GEN: AtomicU64 = AtomicU64::new(0);
static MODE_MANUAL: AtomicBool = AtomicBool::new(false);
static AUDIO_SEL: AtomicUsize = AtomicUsize::new(usize::MAX); // MAX = pista por defecto (original)
static QUALITY_SEL: AtomicI32 = AtomicI32::new(-1); // -1 = automatica
static LAST_POS: AtomicU64 = AtomicU64::new(0);
static LAST_ACT: AtomicU64 = AtomicU64::new(0);
static T_ACT: LazyLock<Instant> = LazyLock::new(Instant::now);

fn data_dir() -> PathBuf {
    let d = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("ytop");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn cookies_path() -> Option<PathBuf> {
    // solo %APPDATA%\ytop\cookies.txt: nunca el directorio actual ni el del exe (podrian ser una copia olvidada)
    let p = data_dir().join("cookies.txt");
    p.exists().then_some(p)
}

/// true para youtube.com / google.com y sus subdominios (no para "notyoutube.com").
fn yt_domain(d: &str) -> bool {
    let d = d.trim_start_matches('.');
    d == "youtube.com" || d.ends_with(".youtube.com") || d == "google.com" || d.ends_with(".google.com")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Convierte lo que se copio del navegador (JSON de una extension, linea "Cookie:" o formato Netscape)
/// al cookies.txt de yt-dlp. Devuelve el texto y la cantidad de cookies.
fn cookies_to_netscape(txt: &str) -> Result<(String, usize), String> {
    let t = txt.trim().trim_start_matches('\u{feff}');
    let t = t.strip_prefix("Cookie:").or_else(|| t.strip_prefix("cookie:")).unwrap_or(t).trim();
    let far = now_secs() + 365 * 86400;
    let mut lines = vec!["# Netscape HTTP Cookie File".to_string()];
    let mut names: Vec<String> = vec![];
    if t.starts_with('[') || t.starts_with('{') {
        // JSON de una extension (Cookie-Editor, EditThisCookie, ...)
        let v: serde_json::Value = serde_json::from_str(t).map_err(|_| "El texto parece JSON pero no se pudo leer".to_string())?;
        let arr = if let Some(a) = v.as_array() { a.clone() } else { v["cookies"].as_array().cloned().ok_or("No encontre la lista de cookies en el JSON")? };
        for c in arr {
            let domain = c["domain"].as_str().unwrap_or("");
            if !yt_domain(domain) {
                continue;
            }
            let (name, value) = (c["name"].as_str().unwrap_or(""), c["value"].as_str().unwrap_or(""));
            if name.is_empty() {
                continue;
            }
            let sub = domain.starts_with('.') || !c["hostOnly"].as_bool().unwrap_or(false);
            let dom = if sub && !domain.starts_with('.') { format!(".{domain}") } else { domain.to_string() };
            let exp = c["expirationDate"].as_f64().or_else(|| c["expires"].as_f64()).map(|x| x as u64).filter(|x| *x > 0).unwrap_or(far);
            lines.push(format!(
                "{}{}\t{}\t{}\t{}\t{}\t{}\t{}",
                if c["httpOnly"].as_bool().unwrap_or(false) { "#HttpOnly_" } else { "" },
                dom,
                if sub { "TRUE" } else { "FALSE" },
                c["path"].as_str().unwrap_or("/"),
                if c["secure"].as_bool().unwrap_or(true) { "TRUE" } else { "FALSE" },
                exp,
                name,
                value
            ));
            names.push(name.to_string());
        }
    } else if t.contains('\t') {
        // Netscape: se conservan las lineas validas
        for l in t.lines() {
            let l2 = l.strip_prefix("#HttpOnly_").unwrap_or(l);
            if l2.starts_with('#') || l2.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = l2.split('\t').collect();
            if f.len() >= 7 {
                lines.push(l.trim_end().to_string());
                names.push(f[5].to_string());
            }
        }
    } else {
        // linea "nombre=valor; nombre=valor; ..." (encabezado Cookie de las herramientas de desarrollador)
        for part in t.split(';') {
            let Some((k, v)) = part.trim().split_once('=') else { continue };
            if k.is_empty() || k.starts_with("ST-") {
                continue;
            }
            lines.push(format!(".youtube.com\tTRUE\t/\tTRUE\t{far}\t{k}\t{v}"));
            names.push(k.to_string());
        }
    }
    if names.is_empty() {
        return Err("No encontre cookies en el portapapeles. Copialas primero y vuelve a tocar el boton.".into());
    }
    if !names.iter().any(|n| n == "SAPISID" || n == "__Secure-3PAPISID") {
        return Err("Faltan las cookies de sesion (SAPISID). Copia TODAS las cookies de youtube.com con la sesion iniciada.".into());
    }
    let n = names.len();
    Ok((lines.join("\n") + "\n", n))
}

/// Texto del portapapeles de Windows.
fn read_clipboard() -> Option<String> {
    let o = Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw"])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    String::from_utf8(o.stdout).ok()
}

fn friendly(e: &str) -> String {
    if e.contains("Sign in") || e.contains("not a bot") {
        "Sesion vencida: toca 'Iniciar sesion'".into()
    } else {
        format!("Error: {e}")
    }
}

fn ytdlp(args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--no-warnings"]).env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1");
    if let Some(c) = cookies_path() {
        cmd.arg("--cookies").arg(c);
    }
    let out = cmd
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("yt-dlp: {e}"))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        let line = e.lines().map(str::trim).rev().find(|l| l.starts_with("ERROR")).or_else(|| e.lines().map(str::trim).rev().find(|l| !l.is_empty()));
        return Err(match line {
            Some(l) => l.chars().take(120).collect(),
            None => format!("yt-dlp termino con error ({})", out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into())),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// id del video de una URL de miniatura (https://i.ytimg.com/vi/<id>/...).
fn vid_from_thumb(url: &str) -> String {
    url.split("/vi/").nth(1).and_then(|r| r.split('/').next()).unwrap_or("").to_string()
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Videos (o, con `lists`, listas de reproduccion y mixes) de una fuente de yt-dlp.
fn list(source: &str, n: usize, only_music: bool, lists: bool) -> Result<Vec<Track>, String> {
    let n = n.to_string();
    let mut out = String::new();
    // el feed recomendado a veces vuelve vacio; un historial o 'Me gusta' vacio de verdad no se reintenta 5 veces
    for _ in 0..(if source.starts_with(':') { 5 } else { 2 }) {
        out = ytdlp(&[
            "--flat-playlist",
            "--playlist-end",
            &n,
            "--print",
            "%(id)s\t%(title)s\t%(duration)s\t%(channel)s\t%(uploader)s\t%(view_count)s\t%(channel_id)s\t%(ie_key)s\t%(playlist_count)s\t%(thumbnails.0.url)s",
            source,
        ])?;
        if !out.trim().is_empty() {
            break;
        }
    }
    let na = |s: &str| if s == "NA" { String::new() } else { s.to_string() };
    Ok(out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 10 {
                return None;
            }
            let is_list = f[7] == "YoutubeTab";
            let artist = na(if f[3] != "NA" { f[3] } else { f[4] });
            let (title, owner) = if is_list {
                let title = match f[0] {
                    "LL" => "Videos que me gustan".to_string(),
                    "WL" => "Ver más tarde".to_string(),
                    _ => na(f[1]),
                };
                let owner = match artist.as_str() {
                    "" | "View full playlist" => if f[0].starts_with("RD") { "Mix de YouTube".to_string() } else { "Tu biblioteca".to_string() },
                    o => o.to_string(),
                };
                (title, owner)
            } else {
                (na(f[1]), artist)
            };
            let thumb_id = if is_list { vid_from_thumb(f[9]) } else { f[0].to_string() };
            Some(Track {
                id: f[0].to_string(),
                title,
                artist: owner,
                dur: f[2].parse::<f64>().unwrap_or(0.0) as u64,
                channel_id: if is_list { String::new() } else { na(f[6]) },
                views: f[5].parse::<u64>().unwrap_or(0),
                date: String::new(),
                music: false,
                thumb_id,
                kind: is_list as u8,
                count: f[8].parse::<u32>().unwrap_or(0),
            })
        })
        .filter(|t| if t.kind == 1 { lists && !t.id.is_empty() } else { !lists && t.id.len() == 11 })
        .filter(|t| t.kind == 1 || !only_music || (45..=900).contains(&t.dur))
        .collect())
}

fn fmt(ms: u64) -> String {
    let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn near(a: [u8; 3], b: [u8; 3], t: i32) -> bool {
    (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= t)
}

/// Barras uniformes y simetricas (izquierda, derecha, arriba, abajo) en pixeles: las de arte cuadrado o de peliculas.
fn find_bars(img: &image::RgbaImage) -> (u32, u32, u32, u32) {
    let (w, h) = img.dimensions();
    let px = |x: u32, y: u32| {
        let p = img.get_pixel(x, y).0;
        [p[0], p[1], p[2]]
    };
    let (sy, sx) = ((h / 48).max(1) as usize, (w / 48).max(1) as usize);
    let col_bar = |x: u32, r: [u8; 3]| (0..h).step_by(sy).all(|y| near(px(x, y), r, 26));
    let row_bar = |y: u32, r: [u8; 3]| (0..w).step_by(sx).all(|x| near(px(x, y), r, 26));
    let (mut l, mut rr, mut t, mut b) = (0, 0, 0, 0);
    let r0 = px(0, h / 2);
    while l < w / 2 && col_bar(l, r0) {
        l += 1;
    }
    let r1 = px(w - 1, h / 2);
    while rr < w / 2 && col_bar(w - 1 - rr, r1) {
        rr += 1;
    }
    let t0 = px(w / 2, 0);
    while t < h / 2 && row_bar(t, t0) {
        t += 1;
    }
    let b0 = px(w / 2, h - 1);
    while b < h / 2 && row_bar(h - 1 - b, b0) {
        b += 1;
    }
    let sym = |a: u32, c: u32, n: u32| a * 100 >= n * 4 && c * 100 >= n * 4 && a.abs_diff(c) * 10 <= a.max(c) * 3;
    let (l, rr) = if sym(l, rr, w) { (l, rr) } else { (0, 0) };
    let (t, b) = if sym(t, b, h) { (t, b) } else { (0, 0) };
    (l, rr, t, b)
}

/// Deja la imagen en 16:9. Si trae barras (arte cuadrado, pelicula), el contenido queda centrado y
/// atras va la misma imagen ampliada y difuminada. `out_w`: ancho final (por defecto el original).
/// Radio de las esquinas de las miniaturas, como fraccion del ancho (12 px en una tarjeta de ~330 px).
const CORNER: f32 = 0.036;
/// Idem para los cuadros de video (16 px sobre ~900 px).
const CORNER_V: f32 = 0.018;

/// Redondea las esquinas de un buffer RGBA: el alfa baja a 0 fuera del arco (con 1 px de suavizado).
/// El renderizador por software de Slint no recorta imagenes con esquinas redondeadas, por eso se hace aca.
fn round_corners(px: &mut [u8], w: usize, h: usize, r: f32) {
    let ri = r.ceil() as usize;
    if ri == 0 || w < 2 * ri || h < 2 * ri {
        return;
    }
    for cy in 0..ri {
        for cx in 0..ri {
            let (dx, dy) = (r - (cx as f32 + 0.5), r - (cy as f32 + 0.5));
            let a = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if a >= 1.0 {
                continue;
            }
            for (x, y) in [(cx, cy), (w - 1 - cx, cy), (cx, h - 1 - cy), (w - 1 - cx, h - 1 - cy)] {
                let i = (y * w + x) * 4 + 3;
                px[i] = (px[i] as f32 * a) as u8;
            }
        }
    }
}

fn to_16x9(img: image::DynamicImage, out_w: Option<u32>) -> image::RgbaImage {
    let mut o = to_16x9_raw(img, out_w);
    let (w, h) = o.dimensions();
    round_corners(&mut o, w as usize, h as usize, w as f32 * CORNER);
    o
}

fn to_16x9_raw(img: image::DynamicImage, out_w: Option<u32>) -> image::RgbaImage {
    use image::imageops::{self, FilterType::Triangle};
    let src = img.to_rgba8();
    let (w, h) = src.dimensions();
    let (l, r, t, b) = find_bars(&src);
    let (cw0, ch0) = (w.saturating_sub(l + r), h.saturating_sub(t + b));
    let has_bars = (l + r > 0 || t + b > 0) && cw0 * 100 >= w * 15 && ch0 * 100 >= h * 15;
    let (cx, cy, cw, ch) = if has_bars {
        (l, t, cw0, ch0)
    } else {
        let hh = (w * 9 / 16).min(h);
        (0, (h - hh) / 2, w, hh)
    };
    let content = imageops::crop_imm(&src, cx, cy, cw, ch).to_image();
    let ow = out_w.unwrap_or(w).max(16);
    let oh = (ow * 9 / 16).max(9);
    let aspect = cw as f32 / ch as f32;
    if (aspect - 16.0 / 9.0).abs() < 0.04 {
        return imageops::resize(&content, ow, oh, Triangle);
    }
    // fondo: la misma imagen llenando el marco (recorte central) y difuminada
    let (bw, bh) = if aspect > 16.0 / 9.0 { (((ch as f32) * 16.0 / 9.0) as u32, ch) } else { (cw, ((cw as f32) * 9.0 / 16.0) as u32) };
    let bgc = imageops::crop_imm(&content, (cw - bw.min(cw)) / 2, (ch - bh.min(ch)) / 2, bw.clamp(1, cw), bh.clamp(1, ch)).to_image();
    let small = imageops::resize(&bgc, 32, 18, Triangle);
    let mut bg = imageops::resize(&small, ow, oh, Triangle);
    for p in bg.pixels_mut() {
        for c in 0..3 {
            p.0[c] = (p.0[c] as u32 * 60 / 100) as u8;
        }
    }
    // frente: el contenido entero, centrado
    let k = (ow as f32 / cw as f32).min(oh as f32 / ch as f32);
    let (fw, fh) = ((((cw as f32) * k) as u32).clamp(1, ow), (((ch as f32) * k) as u32).clamp(1, oh));
    let fg = imageops::resize(&content, fw, fh, Triangle);
    imageops::overlay(&mut bg, &fg, ((ow - fw) / 2) as i64, ((oh - fh) / 2) as i64);
    bg
}

fn fetch_img(agent: &ureq::Agent, id: &str, quality: &str, out_w: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let mut b = Vec::new();
    agent.get(&format!("https://i.ytimg.com/vi/{id}/{quality}.jpg")).call().ok()?.into_reader().take(4 << 20).read_to_end(&mut b).ok()?;
    let img = to_16x9(image::load_from_memory_with_format(&b, image::ImageFormat::Jpeg).ok()?, Some(out_w));
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

/// Caratula grande para la vista de pantalla completa.
fn fetch_art(agent: &ureq::Agent, id: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    fetch_img(agent, id, "maxresdefault", 640).or_else(|| fetch_img(agent, id, "hqdefault", 640))
}

// ---------------------------------------------------------------------
// Presentacion de cada tarjeta (avatar, vistas, fecha)
// ---------------------------------------------------------------------
static UIW: Mutex<Option<Weak<App>>> = Mutex::new(None);

fn fmt_views(n: u64) -> String {
    let dec = |v: f64| format!("{v:.1}").replace('.', ",");
    // los umbrales estan un poco por debajo para que el redondeo no muestre "1000 K" ni "1000 M"
    if n >= 999_500_000 {
        format!("{} mil M vistas", dec(n as f64 / 1e9))
    } else if n >= 999_500 {
        let v = n as f64 / 1e6;
        if v >= 99.5 { format!("{v:.0} M vistas") } else { format!("{} M vistas", dec(v)) }
    } else if n >= 1_000 {
        let v = n as f64 / 1e3;
        if v >= 99.5 { format!("{v:.0} K vistas") } else { format!("{} K vistas", dec(v)) }
    } else {
        format!("{n} vistas")
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "hace 3 dias", "hace 2 meses"... a partir de AAAAMMDD.
fn rel_date(d: &str) -> String {
    if d.len() != 8 {
        return String::new();
    }
    let (Ok(y), Ok(m), Ok(dd)) = (d[0..4].parse::<i64>(), d[4..6].parse::<i64>(), d[6..8].parse::<i64>()) else { return String::new() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_secs() as i64 / 86400).unwrap_or(0);
    let diff = (now - days_from_civil(y, m, dd)).max(0);
    let plural = |n: i64, one: &str, many: &str| if n == 1 { format!("hace 1 {one}") } else { format!("hace {n} {many}") };
    match diff {
        0 => "hoy".to_string(),
        1..=6 => plural(diff, "dia", "dias"),
        7..=29 => plural(diff / 7, "semana", "semanas"),
        30..=359 => plural(diff / 30, "mes", "meses"),
        _ => plural((diff / 365).max(1), "año", "años"),
    }
}

fn meta_text(views: u64, date: &str) -> String {
    let mut p = vec![];
    if views > 0 {
        p.push(fmt_views(views));
    }
    let d = rel_date(date);
    if !d.is_empty() {
        p.push(d);
    }
    p.join(" • ")
}

fn name_color(name: &str) -> (u8, u8, u8) {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let (hue, s, l) = ((h % 360) as f64, 0.5_f64, 0.40_f64);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (hue / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let l = (mx + mn) / 2.0;
    if (mx - mn).abs() < 1e-9 {
        return (0.0, 0.0, l);
    }
    let d = mx - mn;
    let s = if l > 0.5 { d / (2.0 - mx - mn) } else { d / (mx + mn) };
    let h = if mx == r { ((g - b) / d + if g < b { 6.0 } else { 0.0 }) * 60.0 } else if mx == g { ((b - r) / d + 2.0) * 60.0 } else { ((r - g) / d + 4.0) * 60.0 };
    (h, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

/// Color del panel de hover: el tono mas vistoso de la miniatura, muy oscuro (~13% de luminosidad).
fn dominant_tint(buf: &SharedPixelBuffer<Rgba8Pixel>) -> (u8, u8, u8) {
    let (mut sr, mut sg, mut sb, mut sw) = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    for p in buf.as_slice().iter().step_by(7) {
        let (r, g, b) = (p.r as f64 / 255.0, p.g as f64 / 255.0, p.b as f64 / 255.0);
        let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
        let sat = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
        let w = sat * sat * mx + 0.01;
        sr += r * w;
        sg += g * w;
        sb += b * w;
        sw += w;
    }
    let (h, s, _) = rgb_to_hsl(sr / sw, sg / sw, sb / sw);
    hsl_to_rgb(h, (s * 0.8).clamp(0.28, 0.50), 0.135)
}

fn item_for(t: &Track) -> Item {
    let (r, g, b) = name_color(&t.artist);
    let (mut views, mut date, mut music) = (t.views, t.date.clone(), t.music || t.artist.ends_with(" - Topic"));
    if let Some(MetaSt::Done(v, d, m)) = META_ST.lock().unwrap().get(&t.id) {
        if views == 0 {
            views = *v;
        }
        if date.is_empty() {
            date = d.clone();
        }
        music |= *m;
    }
    let initial: String = t.artist.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    let mut it = Item {
        id: t.id.clone().into(),
        title: t.title.clone().into(),
        artist: t.artist.clone().into(),
        dur: fmt(t.dur * 1000).into(),
        meta: meta_text(views, &date).into(),
        is_music: music,
        tint: slint::Color::from_rgb_u8(24, 28, 43),
        initial: initial.into(),
        chan_color: slint::Color::from_rgb_u8(r, g, b),
        ..Default::default()
    };
    if t.kind == 1 {
        it.is_list = true;
        it.dur = if t.id.starts_with("RD") {
            "Mix".into()
        } else if t.count > 0 {
            format!("{} videos", t.count).into()
        } else {
            "Lista".into()
        };
        it.meta = "Ver lista completa".into();
    }
    if let Some(Some(buf)) = AV.lock().unwrap().get(&t.channel_id) {
        it.avatar = Image::from_rgba8(buf.clone());
        it.has_avatar = true;
    }
    it
}

/// Cuando se conocen vistas/fecha de un video (al resolverlo), actualiza su tarjeta.
fn is_music_cat(c: &str) -> bool {
    let c = c.to_lowercase();
    c == "music" || c == "música" || c == "musica"
}

fn notify_meta(id: &str, u: &Urls) {
    if u.views == 0 && u.date.is_empty() && u.category.is_empty() {
        return;
    }
    push_meta(id, u.views, &u.date, is_music_cat(&u.category));
}

/// Guarda vistas/fecha y actualiza la tarjeta de ese video en la pantalla.
fn push_meta(id: &str, views: u64, date: &str, music: bool) {
    META_ST.lock().unwrap().insert(id.to_string(), MetaSt::Done(views, date.to_string(), music));
    let Some(w) = UIW.lock().unwrap().clone() else { return };
    let (id, date) = (id.to_string(), date.to_string());
    let _ = w.upgrade_in_event_loop(move |ui| {
        let m = ui.get_items();
        STATE.with(|s| {
            for (i, t) in s.borrow_mut().view.iter_mut().enumerate() {
                if t.id == id {
                    if views > 0 {
                        t.views = views;
                    }
                    if !date.is_empty() {
                        t.date = date.clone();
                    }
                    if music {
                        t.music = true;
                    }
                    if let Some(mut r) = m.row_data(i) {
                        r.meta = meta_text(t.views, &t.date).into();
                        r.is_music = t.music || t.artist.ends_with(" - Topic");
                        m.set_row_data(i, r);
                    }
                }
            }
        });
    });
}

// ---------------------------------------------------------------------
// Vistas y fecha de cada video (se piden solo para las tarjetas cercanas a lo visible)
// ---------------------------------------------------------------------
enum MetaSt {
    Queued,
    Done(u64, String, bool),
    Failed(Instant),
}

static META_ST: LazyLock<Mutex<HashMap<String, MetaSt>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static METAQ: LazyLock<(Mutex<VecDeque<String>>, Condvar)> = LazyLock::new(|| (Mutex::new(VecDeque::new()), Condvar::new()));

/// (cabecera Cookie, SAPISID) a partir del cookies.txt de la app.
fn yt_auth() -> Option<(String, String)> {
    let txt = std::fs::read_to_string(cookies_path()?).ok()?;
    let (mut pairs, mut sapisid, mut sapisid3) = (vec![], String::new(), String::new());
    for l in txt.lines() {
        let l = l.strip_prefix("#HttpOnly_").unwrap_or(l);
        if l.starts_with('#') || l.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() >= 7 {
            pairs.push(format!("{}={}", f[5], f[6]));
            if f[5] == "SAPISID" {
                sapisid = f[6].to_string();
            } else if f[5] == "__Secure-3PAPISID" {
                sapisid3 = f[6].to_string();
            }
        }
    }
    // la sesion vale con cualquiera de las dos (llevan el mismo valor)
    let key = if sapisid.is_empty() { sapisid3 } else { sapisid };
    (!key.is_empty()).then(|| (pairs.join("; "), key))
}

/// Vistas y fecha (AAAAMMDD) desde el endpoint de reproduccion de YouTube, con la sesion de la app (~40 KB).
fn fetch_meta(agent: &ureq::Agent, id: &str) -> Option<(u64, String, bool)> {
    let (cookie, sapisid) = yt_auth()?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let hash = sha1_smol::Sha1::from(format!("{ts} {sapisid} https://www.youtube.com")).digest().to_string();
    let body = format!(
        r#"{{"context":{{"client":{{"clientName":"WEB","clientVersion":"2.20250925.01.00","hl":"es","gl":"AR"}}}},"videoId":"{id}","contentCheckOk":true,"racyCheckOk":true}}"#
    );
    let resp = agent
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .set("Content-Type", "application/json")
        .set("Origin", "https://www.youtube.com")
        .set("X-Origin", "https://www.youtube.com")
        .set("Cookie", &cookie)
        .set("Authorization", &format!("SAPISIDHASH {ts}_{hash}"))
        .set("X-Goog-AuthUser", "0")
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36")
        .send_string(&body)
        .ok()?;
    let mut txt = String::new();
    resp.into_reader().take(2 << 20).read_to_string(&mut txt).ok()?;
    let j: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let views = j["videoDetails"]["viewCount"].as_str().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let date: String = j["microformat"]["playerMicroformatRenderer"]["publishDate"]
        .as_str()
        .map(|d| d.chars().take(10).filter(|c| c.is_ascii_digit()).collect())
        .unwrap_or_default();
    let date = if date.len() == 8 { date } else { String::new() };
    let music = j["microformat"]["playerMicroformatRenderer"]["category"].as_str().map(is_music_cat).unwrap_or(false);
    (views > 0 || !date.is_empty() || music).then_some((views, date, music))
}

/// Contenido de una lista o mix en una sola peticion al endpoint "next" (unos 0,3 s, sin yt-dlp).
fn fetch_queue(video_id: Option<&str>, playlist_id: &str) -> Option<Vec<Track>> {
    let (cookie, sapisid) = yt_auth().unwrap_or_default();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let hash = sha1_smol::Sha1::from(format!("{ts} {sapisid} https://www.youtube.com")).digest().to_string();
    let vid = video_id.map(|v| format!(r#""videoId":"{v}","#)).unwrap_or_default();
    let body = format!(
        r#"{{"context":{{"client":{{"clientName":"WEB","clientVersion":"2.20250925.01.00","hl":"es","gl":"AR"}}}},{vid}"playlistId":"{playlist_id}"}}"#
    );
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let mut rq = agent
        .post("https://www.youtube.com/youtubei/v1/next?prettyPrint=false")
        .set("Content-Type", "application/json")
        .set("Origin", "https://www.youtube.com")
        .set("X-Origin", "https://www.youtube.com")
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36");
    if !cookie.is_empty() {
        rq = rq.set("Cookie", &cookie).set("Authorization", &format!("SAPISIDHASH {ts}_{hash}")).set("X-Goog-AuthUser", "0");
    }
    let resp = rq.send_string(&body).ok()?;
    let mut txt = String::new();
    resp.into_reader().take(8 << 20).read_to_string(&mut txt).ok()?;
    let j: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let items = j["contents"]["twoColumnWatchNextResults"]["playlist"]["playlist"]["contents"].as_array()?;
    let out: Vec<Track> = items
        .iter()
        .filter_map(|it| {
            let r = &it["playlistPanelVideoRenderer"];
            let id = r["videoId"].as_str()?;
            if id.len() != 11 {
                return None;
            }
            let title = r["title"]["simpleText"].as_str().or_else(|| r["title"]["runs"][0]["text"].as_str())?.to_string();
            let by = &r["shortBylineText"]["runs"][0];
            let dur = r["lengthText"]["simpleText"]
                .as_str()
                .map(|l| l.split(':').filter_map(|x| x.parse::<u64>().ok()).fold(0, |a, x| a * 60 + x))
                .unwrap_or(0);
            Some(Track {
                id: id.to_string(),
                title,
                artist: by["text"].as_str().unwrap_or("").to_string(),
                dur,
                channel_id: by["navigationEndpoint"]["browseEndpoint"]["browseId"].as_str().unwrap_or("").to_string(),
                views: 0,
                date: String::new(),
                music: false,
                thumb_id: id.to_string(),
                kind: 0,
                count: 0,
            })
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

fn request_meta(ids: Vec<String>) {
    let mut st = META_ST.lock().unwrap();
    let (q, cv) = &*METAQ;
    let mut g = q.lock().unwrap();
    for id in ids {
        match st.get(&id) {
            Some(MetaSt::Queued) | Some(MetaSt::Done(..)) => continue,
            Some(MetaSt::Failed(t)) if t.elapsed() < Duration::from_secs(120) => continue,
            _ => {}
        }
        st.insert(id.clone(), MetaSt::Queued);
        g.push_back(id);
    }
    cv.notify_all();
}

fn meta_worker() {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let (q, cv) = &*METAQ;
    loop {
        let id = {
            let mut g = q.lock().unwrap();
            loop {
                if let Some(x) = g.pop_front() {
                    break x;
                }
                g = cv.wait(g).unwrap();
            }
        };
        match fetch_meta(&agent, &id) {
            Some((views, date, music)) => push_meta(&id, views, &date, music),
            None => {
                META_ST.lock().unwrap().insert(id, MetaSt::Failed(Instant::now()));
            }
        }
    }
}

// ---------------------------------------------------------------------
// Avatares de canal: se bajan una vez por canal y quedan en disco
// ---------------------------------------------------------------------
static AV: LazyLock<Mutex<HashMap<String, Option<SharedPixelBuffer<Rgba8Pixel>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static AVQ: LazyLock<(Mutex<VecDeque<String>>, Condvar)> = LazyLock::new(|| (Mutex::new(VecDeque::new()), Condvar::new()));
/// Canales cuyo avatar fallo y cuando: se reintenta a los 5 minutos.
static AVFAIL: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn find_sub(h: &[u8], n: &[u8], from: usize) -> Option<usize> {
    if h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| &h[i..i + n.len()] == n)
}

/// La pagina del canal es grande; la imagen (og:image) aparece a mitad, asi que se corta la lectura ahi.
fn download_avatar(cid: &str) -> Option<Vec<u8>> {
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(20)).build();
    let mut r = agent.get(&format!("https://www.youtube.com/channel/{cid}")).set("Accept-Language", "es").call().ok()?.into_reader();
    let key = b"<meta property=\"og:image\" content=\"";
    let (mut buf, mut chunk) = (Vec::with_capacity(1 << 20), [0u8; 32768]);
    let mut scanned = 0usize;
    let url = loop {
        let n = r.read(&mut chunk).ok()?;
        if n == 0 || buf.len() > (3 << 20) {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = find_sub(&buf, key, scanned) {
            let st = p + key.len();
            if let Some(q) = buf[st..].iter().position(|&c| c == b'"') {
                break String::from_utf8_lossy(&buf[st..st + q]).to_string();
            }
        } else {
            scanned = buf.len().saturating_sub(key.len());
        }
    };
    drop(buf);
    // pedir la version chica (64 px)
    let small = match url.rfind("=s") {
        Some(p) => {
            let rest = &url[p + 2..];
            let d = rest.find('-').unwrap_or(rest.len());
            format!("{}=s64{}", &url[..p], &rest[d..])
        }
        None => url,
    };
    let mut img = Vec::new();
    agent.get(&small).call().ok()?.into_reader().take(1 << 20).read_to_end(&mut img).ok()?;
    (!img.is_empty()).then_some(img)
}

fn load_avatar(cid: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let path = data_dir().join("av").join(format!("{cid}.img"));
    let bytes = match std::fs::read(&path) {
        Ok(b) if !b.is_empty() => b,
        _ => {
            let b = download_avatar(cid)?;
            let _ = std::fs::create_dir_all(path.parent()?);
            let _ = std::fs::write(&path, &b);
            b
        }
    };
    let mut img = image::load_from_memory(&bytes).ok()?.resize_exact(64, 64, image::imageops::FilterType::Triangle).to_rgba8();
    round_corners(&mut img, 64, 64, 32.0); // circulo
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), 64, 64))
}

fn avatar_worker() {
    let (q, cv) = &*AVQ;
    loop {
        let cid = {
            let mut g = q.lock().unwrap();
            loop {
                if let Some(c) = g.pop_front() {
                    break c;
                }
                g = cv.wait(g).unwrap();
            }
        };
        if AV.lock().unwrap().contains_key(&cid) {
            continue;
        }
        if AVFAIL.lock().unwrap().get(&cid).map(|t| t.elapsed() < Duration::from_secs(300)).unwrap_or(false) {
            continue;
        }
        let buf = load_avatar(&cid);
        match &buf {
            Some(_) => {
                AV.lock().unwrap().insert(cid.clone(), buf.clone());
            }
            None => {
                AVFAIL.lock().unwrap().insert(cid.clone(), Instant::now());
            }
        }
        if let (Some(buf), Some(w)) = (buf, UIW.lock().unwrap().clone()) {
            let _ = w.upgrade_in_event_loop(move |ui| {
                let m = ui.get_items();
                STATE.with(|s| {
                    for (i, t) in s.borrow().view.iter().enumerate() {
                        if t.channel_id == cid {
                            if let Some(mut r) = m.row_data(i) {
                                r.avatar = Image::from_rgba8(buf.clone());
                                r.has_avatar = true;
                                m.set_row_data(i, r);
                            }
                        }
                    }
                });
            });
        }
    }
}

fn request_avatars(tracks: &[Track]) {
    let (q, cv) = &*AVQ;
    let mut g = q.lock().unwrap();
    let known = AV.lock().unwrap();
    for t in tracks {
        if !t.channel_id.is_empty() && !known.contains_key(&t.channel_id) && !g.contains(&t.channel_id) {
            g.push_back(t.channel_id.clone());
        }
    }
    cv.notify_all();
}

// ---------------------------------------------------------------------
// Miniaturas con carga perezosa: solo las cercanas a la zona visible
// ---------------------------------------------------------------------
static JPEGS: LazyLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static THUMBQ: LazyLock<(Mutex<VecDeque<(u64, usize, String, u8)>>, Condvar)> = LazyLock::new(|| (Mutex::new(VecDeque::new()), Condvar::new()));
static WIN_LO: AtomicUsize = AtomicUsize::new(0);
static THUMB_TIER: AtomicUsize = AtomicUsize::new(0);
static WIN_HI: AtomicUsize = AtomicUsize::new(0);

/// JPEG comprimido (~10-40 KB) en cache: volver a una miniatura evicta cuesta solo decodificar.
/// `tier` elige la calidad segun el tamaño de la tarjeta: 0 = 320x180, 1 = 480x270, 2 = 640x360.
fn thumb_bytes(agent: &ureq::Agent, id: &str, tier: u8) -> Option<Arc<Vec<u8>>> {
    let key = format!("{tier}{id}");
    if let Some(b) = JPEGS.lock().unwrap().get(&key) {
        return Some(b.clone());
    }
    let q = match tier {
        0 => "mqdefault",
        1 => "hqdefault",
        _ => "sddefault",
    };
    let mut b = Vec::new();
    agent.get(&format!("https://i.ytimg.com/vi/{id}/{q}.jpg")).call().ok()?.into_reader().take(2 << 20).read_to_end(&mut b).ok()?;
    let b = Arc::new(b);
    let mut g = JPEGS.lock().unwrap();
    if g.len() > 400 {
        g.clear();
    }
    g.insert(key, b.clone());
    Some(b)
}

/// Decodifica la miniatura y la deja en 16:9 (con fondo difuminado si trae barras).
fn decode_thumb(b: &[u8], _tier: u8) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = to_16x9(image::load_from_memory_with_format(b, image::ImageFormat::Jpeg).ok()?, None);
    Some(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height()))
}

fn thumb_worker() {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
    let (q, cv) = &*THUMBQ;
    loop {
        let (gen, idx, id, tier) = {
            let mut g = q.lock().unwrap();
            loop {
                if let Some(x) = g.pop_front() {
                    break x;
                }
                g = cv.wait(g).unwrap();
            }
        };
        if THUMB_GEN.load(Relaxed) != gen {
            continue;
        }
        let in_win = idx >= WIN_LO.load(Relaxed) && idx < WIN_HI.load(Relaxed);
        let buf = if in_win { thumb_bytes(&agent, &id, tier).and_then(|b| decode_thumb(&b, tier)).map(|b| { let t = dominant_tint(&b); (b, t) }) } else { None };
        let Some(w) = UIW.lock().unwrap().clone() else { continue };
        let _ = w.upgrade_in_event_loop(move |ui| {
            if THUMB_GEN.load(Relaxed) != gen {
                return;
            }
            let wanted = STATE.with(|s| {
                let mut s = s.borrow_mut();
                let ok = s.view.get(idx).map(|t| t.thumb_id == id).unwrap_or(false) && s.thumb_state.get(idx) == Some(&1);
                if ok {
                    s.thumb_state[idx] = if buf.is_some() { 2 } else { 0 };
                }
                ok
            });
            if let (true, Some((buf, tint))) = (wanted, buf) {
                let m = ui.get_items();
                if let Some(mut r) = m.row_data(idx) {
                    r.thumb = Image::from_rgba8(buf);
                    r.loaded = true;
                    r.tint = slint::Color::from_rgb_u8(tint.0, tint.1, tint.2);
                    m.set_row_data(idx, r);
                }
            }
        });
    }
}

/// Carga las miniaturas de la zona visible (+ margen) y libera las que quedaron lejos.
fn update_thumbs(ui: &App) {
    let m = ui.get_items();
    let n = m.row_count();
    if n == 0 {
        return;
    }
    // calidad de miniatura segun el ancho de la tarjeta; si cambia (ventana mas grande) se recargan
    let cw = ui.get_cw();
    let tier: u8 = if cw > 560.0 { 2 } else if cw > 380.0 { 1 } else { 0 };
    if THUMB_TIER.swap(tier as usize, Relaxed) != tier as usize {
        THUMB_GEN.fetch_add(1, Relaxed);
        let loaded: Vec<usize> = STATE.with(|s| {
            let mut s = s.borrow_mut();
            let v: Vec<usize> = (0..s.thumb_state.len()).filter(|&i| s.thumb_state[i] != 0).collect();
            s.thumb_state.iter_mut().for_each(|x| *x = 0);
            v
        });
        for i in loaded {
            if let Some(mut r) = m.row_data(i) {
                r.thumb = Image::default();
                r.loaded = false;
                m.set_row_data(i, r);
            }
        }
    }
    let cols = ui.get_cols().max(1) as usize;
    let rowh = ui.get_rowh().max(1.0);
    let (y, vh) = (ui.get_scroll_y(), ui.get_view_h().max(1.0));
    let top = ui.get_grid_top();
    let first_row = ((y - top) / rowh).floor().max(0.0) as usize;
    let last_row = ((y + vh - top) / rowh).ceil().max(0.0) as usize;
    let lo = first_row.saturating_sub(1) * cols;
    let hi = ((last_row + 2) * cols).min(n);
    WIN_LO.store(lo, Relaxed);
    WIN_HI.store(hi, Relaxed);
    let gen = THUMB_GEN.load(Relaxed);
    let (elo, ehi) = (lo.saturating_sub(2 * cols), (hi + 2 * cols).min(n));
    let (mut jobs, mut evict, mut meta_ids) = (vec![], vec![], vec![]);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        for i in 0..n.min(s.thumb_state.len()).min(s.view.len()) {
            if i >= lo && i < hi {
                if s.view[i].kind == 0 && (s.view[i].views == 0 || s.view[i].date.is_empty()) {
                    meta_ids.push(s.view[i].id.clone());
                }
                if s.thumb_state[i] == 0 {
                    s.thumb_state[i] = 1;
                    jobs.push((gen, i, s.view[i].thumb_id.clone(), tier));
                }
            } else if (i < elo || i >= ehi) && s.thumb_state[i] == 2 {
                s.thumb_state[i] = 0;
                evict.push(i);
            }
        }
    });
    if !meta_ids.is_empty() {
        request_meta(meta_ids);
    }
    for i in evict {
        if let Some(mut r) = m.row_data(i) {
            r.thumb = Image::default();
            r.loaded = false;
            m.set_row_data(i, r);
        }
    }
    if !jobs.is_empty() {
        let (q, cv) = &*THUMBQ;
        let mut g = q.lock().unwrap();
        g.retain(|x| x.0 == gen);
        g.extend(jobs);
        cv.notify_all();
    }
}

// ---------------------------------------------------------------------
// Listas: mostrar, pestañas, busqueda y "cargar mas"
// ---------------------------------------------------------------------
fn set_view(ui: &App, tracks: Vec<Track>) {
    THUMB_GEN.fetch_add(1, Relaxed);
    ui.invoke_scroll_top();
    let items: Vec<Item> = tracks.iter().map(item_for).collect();
    ui.set_items(ModelRc::new(VecModel::from(items)));
    if ui.get_tab() != 3 {
        for t in tracks.iter().filter(|t| t.kind == 0).take(4) {
            prefetch(&t.id, false);
        }
    }
    request_avatars(&tracks);
    ui.set_can_more(!tracks.is_empty());
    audio::trace(&format!("lista nueva: {} videos", tracks.len()));
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.thumb_state = vec![0; tracks.len()];
        s.view = tracks;
    });
    update_thumbs(ui);
}

/// `gen`: identificador de la peticion (VIEW_GEN al lanzarla); si ya hay una mas nueva, el resultado se descarta.
fn show_result(ui: &App, tab: i32, r: Result<Vec<Track>, String>, cache: bool, gen: u64) {
    if VIEW_GEN.load(Relaxed) != gen {
        return;
    }
    ui.set_loading(false);
    match r {
        Ok(t) => {
            if cache {
                STATE.with(|s| s.borrow_mut().cache.insert(tab, t.clone()));
            }
            if ui.get_tab() == tab {
                ui.set_status(if t.is_empty() { "Sin resultados".into() } else { "".into() });
                if tab == 5 {
                    ui.set_list_count(format!("{} videos", t.len()).into());
                }
                set_view(ui, t);
            }
        }
        Err(e) => ui.set_status(friendly(&e).into()),
    }
}

fn load_tab(ui: &App, tab: i32) {
    ui.set_list_mode(false);
    ui.set_search_kind(0);
    ui.set_tab(tab);
    let (heading, source, n) = match tab {
        0 => ("Recomendado para vos", ":ytrec", 40),
        3 => ("Historial de YouTube", "https://www.youtube.com/feed/history", 40),
        4 => ("Tus listas y mixes", "https://www.youtube.com/feed/playlists", 60),
        _ => ("Tus Me gusta", "https://www.youtube.com/playlist?list=LM", 40),
    };
    ui.set_heading(heading.into());
    STATE.with(|s| s.borrow_mut().src = Some(ViewSrc { tab, source: source.to_string(), n, music: tab == 0, query: None, lists: tab == 4 }));
    if let Some(c) = STATE.with(|s| s.borrow().cache.get(&tab).cloned()) {
        VIEW_GEN.fetch_add(1, Relaxed); // descarta cualquier peticion todavia en curso
        ui.set_status("".into());
        ui.set_loading(false);
        return set_view(ui, c);
    }
    let gen = VIEW_GEN.fetch_add(1, Relaxed) + 1;
    ui.set_items(ModelRc::new(VecModel::from(vec![])));
    ui.set_can_more(false);
    ui.set_status("".into());
    ui.set_loading(true);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let mut r = list(source, n, tab == 0, tab == 4);
        if tab == 0 {
            if let Ok(v) = r.as_mut() {
                add_mixes(v);
            }
        }
        let _ = w.upgrade_in_event_loop(move |ui| show_result(&ui, tab, r, tab != 3, gen));
    });
}

/// Tarjetas "Mix" intercaladas en el feed: una por artista, sembradas con su primer video.
fn add_mixes(v: &mut Vec<Track>) {
    let mut seen: Vec<String> = vec![];
    let mut mixes: Vec<Track> = vec![];
    for t in v.iter().filter(|t| t.kind == 0 && !t.artist.is_empty()) {
        if mixes.len() >= 3 {
            break;
        }
        if seen.contains(&t.artist) {
            continue;
        }
        seen.push(t.artist.clone());
        let name = t.artist.trim_end_matches(" - Topic").to_string();
        mixes.push(Track {
            id: format!("RD{}", t.id),
            title: format!("Mix – {name}"),
            artist: "Mix de YouTube".to_string(),
            thumb_id: t.id.clone(),
            kind: 1,
            ..t.clone()
        });
    }
    for (k, m) in mixes.into_iter().enumerate() {
        let at = (3 + k * 7).min(v.len());
        v.insert(at, m);
    }
}

static LAST_Q: Mutex<String> = Mutex::new(String::new());

fn do_search(ui: &App, q: String, lists: bool) {
    let q = q.trim().to_string();
    if q.is_empty() {
        return;
    }
    *LAST_Q.lock().unwrap() = q.clone();
    ui.set_list_mode(false);
    ui.set_tab(2);
    ui.set_search_kind(lists as i32);
    ui.set_heading(format!("Resultados para \"{q}\"").into());
    let list_url = format!("https://www.youtube.com/results?search_query={}&sp=EgIQAw%253D%253D", urlenc(&q));
    STATE.with(|s| {
        s.borrow_mut().src = Some(ViewSrc {
            tab: 2,
            source: if lists { list_url.clone() } else { String::new() },
            n: 25,
            music: false,
            query: if lists { None } else { Some(q.clone()) },
            lists,
        })
    });
    let gen = VIEW_GEN.fetch_add(1, Relaxed) + 1;
    ui.set_items(ModelRc::new(VecModel::from(vec![])));
    ui.set_can_more(false);
    ui.set_status("".into());
    ui.set_loading(true);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = if lists { list(&list_url, 25, false, true) } else { list(&format!("ytsearch25:{q}"), 25, false, false) };
        let _ = w.upgrade_in_event_loop(move |ui| show_result(&ui, 2, r, false, gen));
    });
}

// ---------------------------------------------------------------------
// Playlists y mixes: pagina de lista, cola de reproduccion, aleatorio y mix
// ---------------------------------------------------------------------
fn open_list(ui: &App, idx: usize) {
    let Some(t) = STATE.with(|s| s.borrow().view.get(idx).cloned()) else { return };
    if t.kind != 1 {
        return;
    }
    let mix = t.id.starts_with("RD");
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.back = Some(BackView { tab: ui.get_tab(), heading: ui.get_heading().to_string(), tracks: s.view.clone(), src: s.src.clone() });
    });
    ui.set_list_mode(true);
    ui.set_tab(5);
    ui.set_list_title(t.title.clone().into());
    ui.set_list_owner(t.artist.clone().into());
    ui.set_list_kind(if mix { "Mix" } else { "Lista de reproducción" }.into());
    ui.set_list_count("".into());
    ui.set_heading("".into());
    // los mixes se abren desde un video semilla; las listas por su id
    let source = if mix && t.thumb_id.len() == 11 {
        format!("https://www.youtube.com/watch?v={}&list={}", t.thumb_id, t.id)
    } else {
        format!("https://www.youtube.com/playlist?list={}", t.id)
    };
    STATE.with(|s| s.borrow_mut().src = Some(ViewSrc { tab: 5, source: source.clone(), n: 100, music: false, query: None, lists: false }));
    let gen = VIEW_GEN.fetch_add(1, Relaxed) + 1;
    ui.set_items(ModelRc::new(VecModel::from(vec![])));
    ui.set_can_more(false);
    ui.set_status("".into());
    ui.set_loading(true);
    let (seed, lid) = (t.thumb_id.clone(), t.id.clone());
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let fast = if mix && seed.len() == 11 { fetch_queue(Some(&seed), &lid) } else { fetch_queue(None, &lid) };
        let r = match fast {
            Some(v) => Ok(v),
            None => list(&source, 100, false, false),
        };
        let _ = w.upgrade_in_event_loop(move |ui| show_result(&ui, 5, r, false, gen));
    });
}

fn list_back(ui: &App) {
    let Some(b) = STATE.with(|s| s.borrow_mut().back.take()) else { return load_tab(ui, 4) };
    VIEW_GEN.fetch_add(1, Relaxed); // descarta la carga de la lista que se estaba abriendo
    ui.set_list_mode(false);
    ui.set_tab(b.tab);
    ui.set_heading(b.heading.into());
    STATE.with(|s| s.borrow_mut().src = b.src.clone());
    ui.set_status("".into());
    ui.set_loading(false);
    set_view(ui, b.tracks);
}

fn shuffle_vec<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(88172645463325252) | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

static QGEN: AtomicU64 = AtomicU64::new(0);

/// Miniaturas chicas (160x90) de las primeras pistas de la cola.
fn load_queue_thumbs(w: Weak<App>, ids: Vec<(usize, String, String)>) {
    let g = QGEN.fetch_add(1, Relaxed) + 1;
    let ids = Arc::new(ids);
    for k in 0..2 {
        let (w, ids) = (w.clone(), ids.clone());
        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
            let mut i = k;
            while i < ids.len() {
                if QGEN.load(Relaxed) != g {
                    return;
                }
                let (idx, tid, thumb) = ids[i].clone();
                let buf = thumb_bytes(&agent, &thumb, 0).and_then(|b| {
                    let img = to_16x9(image::load_from_memory_with_format(&b, image::ImageFormat::Jpeg).ok()?, Some(160));
                    Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(img.as_raw(), 160, 90))
                });
                if let Some(buf) = buf {
                    let _ = w.upgrade_in_event_loop(move |ui| {
                        if QGEN.load(Relaxed) != g {
                            return;
                        }
                        let m = ui.get_queue_items();
                        if let Some(mut r) = m.row_data(idx) {
                            if r.id.as_str() == tid {
                                r.thumb = Image::from_rgba8(buf);
                                r.loaded = true;
                                m.set_row_data(idx, r);
                            }
                        }
                    });
                }
                i += 2;
            }
        });
    }
}

/// Indice (en la cola completa) del primer item que muestra el panel de cola.
static QBASE: AtomicUsize = AtomicUsize::new(0);

/// Muestra la cola actual (lista, mix o lo que se estaba viendo) en el panel del reproductor.
/// Se muestran hasta 200 items alrededor de la pista actual.
fn publish_queue(ui: &App) {
    let (items, ids, title, cur, base) = STATE.with(|s| {
        let s = s.borrow();
        let cur = s.cur.unwrap_or(0);
        let base = if cur >= 150 { cur - 100 } else { 0 };
        let items: Vec<Item> = s.queue.iter().skip(base).take(200).map(item_for).collect();
        let ids: Vec<(usize, String, String)> = s.queue.iter().skip(base).take(60).enumerate().map(|(i, t)| (i, t.id.clone(), t.thumb_id.clone())).collect();
        (items, ids, s.queue_title.clone(), cur, base)
    });
    QBASE.store(base, Relaxed);
    ui.set_queue_items(ModelRc::new(VecModel::from(items)));
    ui.set_queue_title(title.into());
    ui.set_queue_cur((cur - base) as i32);
    load_queue_thumbs(ui.as_weak(), ids);
}

/// "Reproducir todo" / "Aleatorio" sobre la lista que se esta viendo.
fn start_queue_from_view(ui: &App, sh: &Arc<Shared>, shuffle: bool) {
    let mut q = STATE.with(|s| s.borrow().view.clone());
    q.retain(|t| t.kind == 0);
    if q.is_empty() {
        return;
    }
    if shuffle {
        shuffle_vec(&mut q);
    }
    let title = if ui.get_list_mode() { ui.get_list_title().to_string() } else { ui.get_heading().to_string() };
    stop_preview(Some(ui));
    MODE_MANUAL.store(false, Relaxed);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.queue = q;
        s.queue_title = if shuffle { format!("{title} · aleatorio") } else { title };
    });
    start(ui, sh, 0);
    publish_queue(ui);
    ui.set_full(true);
}

/// Arma un mix (radio) a partir del video que suena y lo deja como cola.
fn start_mix(ui: &App) {
    let Some(t) = STATE.with(|s| {
        let s = s.borrow();
        s.cur.and_then(|c| s.queue.get(c)).cloned()
    }) else {
        return;
    };
    ui.set_status("".into());
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let r = match fetch_queue(Some(&t.id), &format!("RD{}", t.id)) {
            Some(v) => Ok(v),
            None => list(&format!("https://www.youtube.com/watch?v={}&list=RD{}", t.id, t.id), 50, false, false),
        };
        let _ = w.upgrade_in_event_loop(move |ui| {
            ui.set_status("".into());
            let same = STATE.with(|s| {
                let s = s.borrow();
                s.cur.and_then(|c| s.queue.get(c)).map(|x| x.id == t.id).unwrap_or(false)
            });
            if !same {
                return; // se cambio de pista mientras se armaba el mix
            }
            match r {
                Ok(mut v) if !v.is_empty() => {
                    if v[0].id != t.id {
                        v.insert(0, t.clone());
                    }
                    let title = format!("Mix · {}", if t.artist.is_empty() { t.title.clone() } else { t.artist.clone() });
                    STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        s.queue = v;
                        s.cur = Some(0);
                        s.queue_title = title;
                    });
                    publish_queue(&ui);
                    ui.set_queue_open(true);
                }
                Ok(_) => ui.set_status("No se pudo armar el mix de este video".into()),
                Err(e) => ui.set_status(friendly(&e).into()),
            }
        });
    });
}

static MORE_BUSY: AtomicBool = AtomicBool::new(false);

/// Agrega al final los videos nuevos (sin repetir) y sigue cargando solo lo visible.
fn append_view(ui: &App, new: Vec<Track>, n: usize) {
    let new_len = new.len();
    let m = ui.get_items();
    if let Some(vm) = m.as_any().downcast_ref::<VecModel<Item>>() {
        for t in &new {
            vm.push(item_for(t));
        }
    }
    request_avatars(&new);
    let tab = ui.get_tab();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.view.extend(new.iter().cloned());
        let len = s.view.len();
        s.thumb_state.resize(len, 0);
        if let Some(src) = s.src.as_mut() {
            src.n = n;
        }
        if tab == 0 || tab == 1 || tab == 4 {
            let v = s.view.clone();
            s.cache.insert(tab, v);
        }
    });
    audio::trace(&format!("cargar mas: +{} videos", new_len));
    update_thumbs(ui);
}

fn load_more(ui: &App) {
    if MORE_BUSY.swap(true, Relaxed) {
        return;
    }
    let Some(src) = STATE.with(|s| s.borrow().src.clone()) else {
        MORE_BUSY.store(false, Relaxed);
        return;
    };
    ui.set_loading_more(true);
    let gen = VIEW_GEN.load(Relaxed);
    let (tab, n) = (src.tab, src.n + 30);
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let source = match &src.query {
            Some(q) => format!("ytsearch{n}:{q}"),
            None => src.source.clone(),
        };
        let r = list(&source, n, src.music, src.lists);
        let _ = w.upgrade_in_event_loop(move |ui| {
            MORE_BUSY.store(false, Relaxed);
            ui.set_loading_more(false);
            if VIEW_GEN.load(Relaxed) != gen || ui.get_tab() != tab {
                return;
            }
            match r {
                Ok(all) => {
                    let known: std::collections::HashSet<String> = STATE.with(|s| s.borrow().view.iter().map(|t| t.id.clone()).collect());
                    let new: Vec<Track> = all.into_iter().filter(|t| !known.contains(&t.id)).collect();
                    if new.is_empty() {
                        ui.set_can_more(false);
                        return;
                    }
                    append_view(&ui, new, n);
                }
                Err(e) => ui.set_status(friendly(&e).into()),
            }
        });
    });
}

// ---------------------------------------------------------------------
// Cache de URLs de stream + precarga (para empezar a sonar casi al instante)
// ---------------------------------------------------------------------
#[derive(Clone, Default)]
struct Urls {
    a: String,  // audio
    v: String,  // video (<=360p)
    v720: String, // video (<=720p) para reproductor grande
    cc: String, // subtitulos (json3), vacio si no hay
    date: String,
    views: u64,
    category: String,
    audio_opts: Vec<AudioOpt>,
    video_opts: Vec<VideoOpt>,
}

/// Una pista de audio (idioma) disponible.
#[derive(Clone, Default)]
struct AudioOpt {
    label: String,
    url: String,
    original: bool,
}

/// Una calidad de video disponible (la mejor por altura).
#[derive(Clone, Default)]
struct VideoOpt {
    height: u32,  // alto real en pixeles (para elegir automaticamente)
    label_h: u32, // lado corto: lo que se muestra (360p, 720p...)
    fps: u32,
    url: String,
}

enum Slot {
    Pending,
    Done(Instant, Urls),
}

enum St {
    Hit(Urls),
    Wait,
    Miss,
}

static URLS: LazyLock<Mutex<HashMap<String, Slot>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static URLS_CV: Condvar = Condvar::new();
static HOVER_GEN: AtomicU64 = AtomicU64::new(0);

const AUDIO_FMT: &str = "140/bestaudio[ext=m4a]";
const VIDEO_FMT: &str = "bv*[height<=360][vcodec^=avc1]/bv*[height<=480][vcodec^=avc1]/bv*[height<=480]/bv*[height<=720]";
const VIDEO720_FMT: &str = "bv*[height<=720][vcodec^=avc1]/bv*[height<=720]";
const URL_TTL: Duration = Duration::from_secs(7200);

/// Elige la mejor pista de subtitulos: manual es > auto es > manual en > auto en.
fn pick_cc(manual: &serde_json::Value, auto_es: &serde_json::Value, auto_en: &serde_json::Value) -> String {
    let json3 = |v: &serde_json::Value| -> Option<String> {
        v.as_array()?.iter().find(|f| f["ext"] == "json3").and_then(|f| f["url"].as_str()).map(String::from)
    };
    let manual_lang = |pre: &str| -> Option<String> {
        manual.as_object()?.iter().filter(|(k, _)| k.starts_with(pre)).find_map(|(_, v)| json3(v))
    };
    manual_lang("es").or_else(|| json3(auto_es)).or_else(|| manual_lang("en")).or_else(|| json3(auto_en)).unwrap_or_default()
}

fn parse_urls(out: &str) -> Urls {
    let mut u = Urls::default();
    let (mut manual, mut es, mut en) = (serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null);
    let json = |t: &str| serde_json::from_str::<serde_json::Value>(t).unwrap_or_default();
    for l in out.lines().map(str::trim) {
        if let Some(f) = l.strip_prefix("F:") {
            let mut it = f.splitn(3, '|');
            let (_vc, h, url) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
            if url.starts_with("http") {
                if url.contains("mime=video") {
                    if h.parse::<u32>().unwrap_or(0) > 360 {
                        if u.v720.is_empty() {
                            u.v720 = url.to_string();
                        }
                    } else if u.v.is_empty() {
                        u.v = url.to_string();
                    }
                } else if u.a.is_empty() {
                    u.a = url.to_string();
                }
            }
        } else if let Some(j) = l.strip_prefix("S:") {
            if manual.is_null() {
                manual = json(j);
            }
        } else if let Some(j) = l.strip_prefix("E:") {
            if es.is_null() {
                es = json(j);
            }
        } else if let Some(j) = l.strip_prefix("N:") {
            if en.is_null() {
                en = json(j);
            }
        } else if let Some(tj) = l.strip_prefix("T:") {
            if u.audio_opts.is_empty() && u.video_opts.is_empty() {
                parse_formats(tj, &mut u);
            }
        } else if let Some(m) = l.strip_prefix("M:") {
            let f: Vec<&str> = m.split('|').collect();
            if f.len() >= 2 {
                if u.date.is_empty() && f[0] != "NA" {
                    u.date = f[0].to_string();
                }
                if u.views == 0 {
                    u.views = f[1].parse().unwrap_or(0);
                }
            }
            if f.len() >= 3 && f[2] != "NA" && u.category.is_empty() {
                u.category = f[2].to_string();
            }
        }
    }
    u.cc = pick_cc(&manual, &es, &en);
    u
}

fn lang_name(code: &str) -> Option<&'static str> {
    Some(match code.split('-').next().unwrap_or("") {
        "es" => "Español",
        "en" => "Inglés",
        "pt" => "Portugués",
        "fr" => "Francés",
        "de" => "Alemán",
        "it" => "Italiano",
        "ja" => "Japonés",
        "ko" => "Coreano",
        "zh" => "Chino",
        "ru" => "Ruso",
        "ar" => "Árabe",
        "hi" => "Hindi",
        "tr" => "Turco",
        "pl" => "Polaco",
        "id" => "Indonesio",
        "vi" => "Vietnamita",
        "th" => "Tailandés",
        "bn" => "Bengalí",
        "ta" => "Tamil",
        "te" => "Telugu",
        "ml" => "Malayalam",
        "mr" => "Maratí",
        "pa" => "Panyabí",
        "nl" => "Neerlandés",
        "uk" => "Ucraniano",
        "sv" => "Sueco",
        "he" => "Hebreo",
        "el" => "Griego",
        "cs" => "Checo",
        "ro" => "Rumano",
        "hu" => "Húngaro",
        "fil" => "Filipino",
        _ => return None,
    })
}

/// "Español", "Inglés (US) · original", etc. a partir del codigo de idioma y la nota de yt-dlp.
fn audio_label(lang: &str, note: &str) -> String {
    let first = note.split(',').next().unwrap_or("").trim();
    let original = note.contains("original");
    let region = first.find('(').and_then(|a| first[a + 1..].find(')').map(|b| first[a + 1..a + 1 + b].to_string())).filter(|r| r != "default");
    let mut s = match lang_name(lang) {
        Some(n) => n.to_string(),
        None => first.split(" original").next().unwrap_or(first).split('(').next().unwrap_or(first).trim().to_string(),
    };
    if s.is_empty() {
        s = "Audio".to_string();
    }
    if let Some(r) = region {
        s = format!("{s} ({r})");
    }
    if original {
        s.push_str(" · original");
    }
    s
}

fn qlabel(o: &VideoOpt) -> String {
    if o.fps > 30 {
        format!("{}p{}", o.label_h, o.fps)
    } else {
        format!("{}p", o.label_h)
    }
}

/// Lista de pistas de audio (m4a, una por idioma, la original primero) y de calidades (la mejor por altura).
fn parse_formats(j: &str, u: &mut Urls) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else { return };
    let Some(arr) = v.as_array() else { return };
    let mut audio: Vec<(String, f64, AudioOpt)> = vec![]; // (idioma, kbps, pista): una por idioma, la de mayor calidad
    for f in arr {
        let id = f["format_id"].as_str().unwrap_or("");
        let url = f["url"].as_str().unwrap_or("");
        if f["ext"].as_str() != Some("m4a") || f["vcodec"].as_str().unwrap_or("none") != "none" || url.is_empty() || id.contains("drc") {
            continue;
        }
        let lang = f["language"].as_str().unwrap_or("").to_string();
        let tbr = f["tbr"].as_f64().unwrap_or(0.0);
        let note = f["format_note"].as_str().unwrap_or("");
        let opt = AudioOpt { label: audio_label(&lang, note), url: url.to_string(), original: note.contains("original") };
        match audio.iter_mut().find(|(l, _, _)| *l == lang) {
            Some(e) if tbr > e.1 => *e = (lang, tbr, opt),
            Some(_) => {}
            None => audio.push((lang, tbr, opt)),
        }
    }
    let mut audio: Vec<AudioOpt> = audio.into_iter().map(|(_, _, o)| o).collect();
    audio.sort_by(|a, b| b.original.cmp(&a.original).then(a.label.cmp(&b.label)));
    u.audio_opts = audio;

    // video: por (altura, 60fps o no) el mejor codec que decodifique ffmpeg (avc1, luego vp9, luego av1)
    let mut best: HashMap<(u32, bool), (u8, f64, VideoOpt)> = HashMap::new();
    for f in arr {
        let (vc, ac) = (f["vcodec"].as_str().unwrap_or("none"), f["acodec"].as_str().unwrap_or("none"));
        let (h, url) = (f["height"].as_u64().unwrap_or(0) as u32, f["url"].as_str().unwrap_or(""));
        if vc == "none" || ac != "none" || h == 0 || url.is_empty() || h > 2160 {
            continue;
        }
        let rank = if vc.starts_with("avc1") { 0 } else if vc.starts_with("vp9") || vc.starts_with("vp09") { 1 } else { 2 };
        let fps = f["fps"].as_f64().unwrap_or(30.0);
        let tbr = f["tbr"].as_f64().unwrap_or(0.0);
        let w = f["width"].as_u64().unwrap_or(0) as u32;
        let lh = if w > 0 { w.min(h) } else { h };
        let key = (lh, fps > 30.0);
        let opt = VideoOpt { height: h, label_h: lh, fps: fps.round() as u32, url: url.to_string() };
        match best.get(&key) {
            Some((r, t, _)) if *r < rank || (*r == rank && *t >= tbr) => {}
            _ => {
                best.insert(key, (rank, tbr, opt));
            }
        }
    }
    let mut vids: Vec<VideoOpt> = best.into_values().map(|(_, _, o)| o).collect();
    vids.sort_by(|a, b| b.label_h.cmp(&a.label_h).then(b.fps.cmp(&a.fps)));
    u.video_opts = vids;
}

/// Una sola consulta a yt-dlp trae las URLs de audio y video y las pistas de subtitulos.
fn fetch_urls(id: &str) -> Result<Urls, String> {
    let page = format!("https://www.youtube.com/watch?v={id}");
    let prints = ["--print", "F:%(vcodec)s|%(height)s|%(url)s", "--print", "S:%(subtitles)j", "--print", "E:%(automatic_captions.es)j", "--print", "N:%(automatic_captions.en)j", "--print", "M:%(upload_date)s|%(view_count)s|%(categories.0)s", "--print", "pre_process:T:%(formats.:.{format_id,language,format_note,ext,acodec,vcodec,width,height,fps,tbr,url})j"];
    let run = |fmt: &str| {
        let mut args: Vec<&str> = vec!["--no-playlist", "-f", fmt];
        args.extend_from_slice(&prints);
        args.push(&page);
        ytdlp(&args)
    };
    match run(&format!("{AUDIO_FMT},{VIDEO_FMT},{VIDEO720_FMT}")) {
        Ok(out) => {
            let u = parse_urls(&out);
            if !u.a.is_empty() {
                return Ok(u);
            }
        }
        // si el problema es la sesion, no tiene sentido reintentar
        Err(e) if e.contains("Sign in") || e.contains("not a bot") => return Err(e),
        Err(_) => {}
    }
    let u = parse_urls(&run(AUDIO_FMT)?);
    if u.a.is_empty() {
        Err("sin URL de stream".to_string())
    } else {
        Ok(u)
    }
}

/// URLs con cache; si otro hilo ya las esta resolviendo, espera.
fn resolve(id: &str) -> Result<Urls, String> {
    let mut g = URLS.lock().unwrap();
    loop {
        let st = match g.get(id) {
            Some(Slot::Done(t, u)) if t.elapsed() < URL_TTL => St::Hit(u.clone()),
            Some(Slot::Pending) => St::Wait,
            _ => St::Miss,
        };
        match st {
            St::Hit(u) => return Ok(u),
            St::Wait => g = URLS_CV.wait(g).unwrap(),
            St::Miss => break,
        }
    }
    g.insert(id.to_string(), Slot::Pending);
    drop(g);
    let r = fetch_urls(id);
    let mut g = URLS.lock().unwrap();
    match &r {
        Ok(u) => {
            if g.len() > 80 {
                g.retain(|_, s| match s {
                    Slot::Done(t, ..) => t.elapsed() < Duration::from_secs(3600),
                    Slot::Pending => true,
                });
            }
            g.insert(id.to_string(), Slot::Done(Instant::now(), u.clone()));
        }
        Err(_) => {
            g.remove(id);
        }
    }
    URLS_CV.notify_all();
    if let Ok(u) = &r {
        notify_meta(id, u);
    }
    r
}

fn invalidate(id: &str) {
    URLS.lock().unwrap().remove(id);
}

// ---------------------------------------------------------------------
// Subtitulos
// ---------------------------------------------------------------------
struct Cue {
    start: u64,
    end: u64,
    text: String,
}

/// (id del video, lineas ordenadas por inicio)
static CUES: Mutex<(String, Vec<Cue>)> = Mutex::new((String::new(), Vec::new()));

fn cue_at(pos: u64) -> String {
    let g = CUES.lock().unwrap();
    let i = g.1.partition_point(|c| c.start <= pos);
    match i.checked_sub(1).map(|k| &g.1[k]) {
        Some(c) if pos < c.end => c.text.clone(),
        _ => String::new(),
    }
}

/// Marca si hay subtitulos y, si `want`, los descarga.
fn load_cues(w: Weak<App>, id: String, want: bool) {
    std::thread::spawn(move || {
        let Ok(u) = resolve(&id) else { return };
        let avail = !u.cc.is_empty();
        let id2 = id.clone();
        let _ = w.upgrade_in_event_loop(move |ui| {
            if ui.get_playing_id().as_str() == id2 {
                ui.set_cc_available(avail);
            }
        });
        if !avail || !want || CUES.lock().unwrap().0 == id {
            return;
        }
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
        let Ok(resp) = agent.get(&u.cc).call() else { return };
        let mut body = String::new();
        if resp.into_reader().take(4 << 20).read_to_string(&mut body).is_err() {
            return;
        }
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&body) else { return };
        let mut cues: Vec<Cue> = j["events"]
            .as_array()
            .map(|ev| {
                ev.iter()
                    .filter_map(|e| {
                        let text: String = e["segs"].as_array()?.iter().filter_map(|s| s["utf8"].as_str()).collect();
                        let text = text.replace('\n', " ").trim().to_string();
                        if text.is_empty() {
                            return None;
                        }
                        let start = e["tStartMs"].as_u64()?;
                        Some(Cue { start, end: start + e["dDurationMs"].as_u64().unwrap_or(3000), text })
                    })
                    .collect()
            })
            .unwrap_or_default();
        cues.sort_by_key(|c| c.start);
        audio::trace(&format!("subtitulos: {} lineas", cues.len()));
        let _ = w.upgrade_in_event_loop(move |ui| {
            // solo si la pista sigue siendo la misma (si no, se mostrarian subtitulos ajenos)
            if ui.get_playing_id().as_str() == id {
                *CUES.lock().unwrap() = (id, cues);
            }
        });
    });
}

static PREFETCH: LazyLock<(Mutex<VecDeque<String>>, Condvar)> = LazyLock::new(|| (Mutex::new(VecDeque::new()), Condvar::new()));

/// Encola la resolucion anticipada del audio de `id` (`front` = prioridad).
fn prefetch(id: &str, front: bool) {
    if matches!(URLS.lock().unwrap().get(id), Some(Slot::Pending) | Some(Slot::Done(..))) {
        return;
    }
    let (q, cv) = &*PREFETCH;
    let mut q = q.lock().unwrap();
    q.retain(|x| x != id);
    if front {
        q.push_front(id.to_string());
    } else {
        q.push_back(id.to_string());
    }
    q.truncate(24);
    cv.notify_one();
}

fn prefetch_worker() {
    let (q, cv) = &*PREFETCH;
    loop {
        let id = {
            let mut g = q.lock().unwrap();
            loop {
                if let Some(id) = g.pop_front() {
                    break id;
                }
                g = cv.wait(g).unwrap();
            }
        };
        let _ = resolve(&id);
    }
}

/// Cuenta la reproduccion en el historial de YouTube solo tras ~10 s de escucha real.
fn mark_watched_later(sh: Arc<Shared>, gen: u64, id: String) {
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if sh.gen.load(Relaxed) != gen {
            return;
        }
    }
    let _ = ytdlp(&["--no-playlist", "--simulate", "--mark-watched", "-f", AUDIO_FMT, &format!("https://www.youtube.com/watch?v={id}")]);
}

// ---------------------------------------------------------------------
// Video (ffmpeg -> fotogramas RGBA sincronizados con el audio)
// ---------------------------------------------------------------------
static VGEN: AtomicU64 = AtomicU64::new(0);
static VCHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);
static FRAME_PENDING: AtomicBool = AtomicBool::new(false);
static FFMPEG: LazyLock<Option<PathBuf>> = LazyLock::new(find_ffmpeg);

/// true si `exe` es un ffmpeg moderno: hay "shims" y builds de 2013 en el PATH que fallan con las opciones que usamos.
fn ffmpeg_ok(exe: &std::path::Path) -> bool {
    let Ok(o) = Command::new(exe).arg("-version").stdin(Stdio::null()).creation_flags(0x0800_0000).output() else { return false };
    let t = String::from_utf8_lossy(&o.stdout);
    let Some(v) = t.lines().next().and_then(|l| l.strip_prefix("ffmpeg version ")) else { return false };
    let tok = v.split_whitespace().next().unwrap_or("");
    if let Some(n) = tok.strip_prefix("N-") {
        return n.split('-').next().and_then(|x| x.parse::<u32>().ok()).map(|x| x >= 90_000).unwrap_or(true);
    }
    let major: String = tok.trim_start_matches('n').chars().take_while(|c| c.is_ascii_digit()).collect();
    major.parse::<u32>().map(|m| m >= 4).unwrap_or(true)
}

fn find_ffmpeg() -> Option<PathBuf> {
    let mut c = vec![data_dir().join("ffmpeg.exe")];
    if let Ok(e) = std::env::current_exe() {
        if let Some(d) = e.parent() {
            c.push(d.join("ffmpeg.exe"));
        }
    }
    if let Some(p) = c.into_iter().find(|p| p.exists()) {
        return Some(p);
    }
    // cualquier ffmpeg.exe del PATH que sea moderno
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            let p = d.join("ffmpeg.exe");
            if p.exists() && ffmpeg_ok(&p) {
                return Some(p);
            }
        }
    }
    let out = Command::new("python")
        .args(["-c", "import imageio_ffmpeg;print(imageio_ffmpeg.get_ffmpeg_exe())"])
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    p.exists().then_some(p)
}

fn stop_video() {
    VGEN.fetch_add(1, Relaxed);
    if let Some(mut c) = VCHILD.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn start_video(w: Weak<App>, sh: Arc<Shared>, id: String, start_ms: u64, vw: usize, vh: usize) {
    stop_video();
    let vgen = VGEN.load(Relaxed);
    std::thread::spawn(move || video_thread(w, sh, id, start_ms, vgen, vw, vh));
}

/// Video con ffmpeg ya escalado al tamaño exacto en que se dibuja: la interfaz lo copia 1:1 sin escalarlo.
fn video_thread(w: Weak<App>, sh: Arc<Shared>, id: String, start_ms: u64, vgen: u64, vw: usize, vh: usize) {
    let fps: u64 = if vw * vh > 2_500_000 { 24 } else { 30 };
    let alive = || VGEN.load(Relaxed) == vgen;
    let say = |s: &str| {
        let s = s.to_string();
        let _ = w.upgrade_in_event_loop(move |ui| ui.set_status(s.into()));
    };
    let Some(ff) = FFMPEG.clone() else { return say("No encontre ffmpeg para el video") };
    let (url, qtxt) = match resolve(&id) {
        Ok(u) => match pick_video(&u, vw, vh) {
            Some(x) => x,
            None => return say("Este video no tiene formato de video disponible"),
        },
        Err(e) => return say(&friendly(&e)),
    };
    {
        let w2 = w.clone();
        let _ = w2.upgrade_in_event_loop(move |ui| ui.set_quality_label(qtxt.into()));
    }
    audio::trace("video: URL lista");
    if !alive() {
        return;
    }
    let mut cmd = Command::new(ff);
    cmd.args(["-loglevel", "quiet", "-probesize", "1000000", "-analyzeduration", "1000000", "-hwaccel", "auto"]);
    if start_ms > 500 {
        cmd.args(["-ss", &format!("{:.3}", start_ms as f64 / 1000.0)]);
    }
    let mut child = match cmd
        .args(["-i", &url, "-an", "-sn", "-vf"])
        .arg(format!("fps={fps},scale={vw}:{vh}:flags=bicubic"))
        .args(["-pix_fmt", "rgba", "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return say(&format!("ffmpeg: {e}")),
    };
    audio::trace("video: ffmpeg iniciado");
    let Some(mut out) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    {
        let mut g = VCHILD.lock().unwrap();
        if !alive() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        *g = Some(child);
    }
    let mut k: u64 = 0;
    loop {
        if !alive() {
            return;
        }
        // se lee directo al bufer final (sin copia intermedia)
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(vw as u32, vh as u32);
        if out.read_exact(buf.make_mut_bytes()).is_err() {
            // ffmpeg termino o fallo: se recoge el proceso para no dejarlo colgado
            if alive() {
                if let Some(mut c) = VCHILD.lock().unwrap().take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
            return;
        }
        round_corners(buf.make_mut_bytes(), vw, vh, vw as f32 * CORNER_V);
        let ts = start_ms + k * 1000 / fps;
        k += 1;
        // esperar a que el audio llegue a este instante (o descartar si va tarde)
        loop {
            if !alive() {
                return;
            }
            if sh.pos_ms() + 15 >= ts {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if sh.pos_ms() > ts + 120 || FRAME_PENDING.swap(true, Relaxed) {
            continue;
        }
        if k == 1 {
            audio::trace("video: primer fotograma");
        }
        let _ = w.upgrade_in_event_loop(move |ui| {
            FRAME_PENDING.store(false, Relaxed);
            if VGEN.load(Relaxed) == vgen {
                ui.set_np_frame(Image::from_rgba8(buf));
                ui.set_np_frame_loaded(true);
            }
        });
    }
}

/// El video corre solo con la pantalla completa abierta en modo Video.
fn sync_video(ui: &App, sh: &Arc<Shared>) {
    if ui.get_full() && ui.get_video_mode() {
        audio::trace("video: solicitado");
    }
    let id = STATE.with(|s| {
        let s = s.borrow();
        s.cur.and_then(|c| s.queue.get(c)).map(|t| t.id.clone())
    });
    match id {
        Some(id) if ui.get_full() && ui.get_video_mode() => {
            // tamaño fisico exacto del recuadro de video (par, 16:9, hasta 2560 de ancho)
            let sf = ui.window().scale_factor();
            let vw = ((ui.get_player_w() * sf) as usize).clamp(320, 2560) & !1;
            let vh = (vw * 9 / 16) & !1;
            start_video(ui.as_weak(), sh.clone(), id, sh.pos_ms(), vw, vh)
        }
        _ => stop_video(),
    }
}

// ---------------------------------------------------------------------
// Vista previa al pasar el mouse sobre una tarjeta (video mudo en loop corto)
// ---------------------------------------------------------------------
const PW: usize = 320;
const PH: usize = 180;

static PGEN: AtomicU64 = AtomicU64::new(0);
static PCHILD: Mutex<Option<std::process::Child>> = Mutex::new(None);
static PREV_PENDING: AtomicBool = AtomicBool::new(false);

fn kill_preview_child() {
    if let Some(mut c) = PCHILD.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn stop_preview(ui: Option<&App>) {
    PGEN.fetch_add(1, Relaxed);
    kill_preview_child();
    if let Some(ui) = ui {
        ui.set_preview_id("".into());
    }
}

/// Corre en un hilo aparte: espera las URLs (suelen estar precargadas) y reproduce ~10 s.
fn start_preview(w: Weak<App>, id: String, dur: u64, hover_gen: u64) {
    PGEN.fetch_add(1, Relaxed);
    kill_preview_child();
    let pgen = PGEN.load(Relaxed);
    let alive = || PGEN.load(Relaxed) == pgen && HOVER_GEN.load(Relaxed) == hover_gen;
    let Some(ff) = FFMPEG.clone() else { return };
    let Ok(u) = resolve(&id) else { return };
    if u.v.is_empty() || !alive() {
        return;
    }
    let start = if dur > 40 { (dur / 4).min(dur.saturating_sub(15)) } else { 0 };
    let mut cmd = Command::new(ff);
    cmd.args(["-loglevel", "quiet", "-probesize", "300000", "-analyzeduration", "300000"]);
    if start > 0 {
        cmd.args(["-ss", &start.to_string()]);
    }
    let Ok(mut child) = cmd
        .args(["-re", "-t", "10", "-i", &u.v, "-an", "-sn", "-vf"])
        .arg(format!("fps=12,scale={PW}:{PH}:flags=fast_bilinear"))
        .args(["-pix_fmt", "rgba", "-f", "rawvideo", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn()
    else {
        return;
    };
    let Some(mut out) = child.stdout.take() else { return };
    {
        let mut g = PCHILD.lock().unwrap();
        if !alive() {
            let _ = child.kill();
            return;
        }
        *g = Some(child);
    }
    let mut frame = vec![0u8; PW * PH * 4];
    let mut k: u64 = 0;
    while alive() && out.read_exact(&mut frame).is_ok() {
        k += 1;
        if PREV_PENDING.swap(true, Relaxed) {
            continue;
        }
        let remaining = fmt(dur.saturating_sub(start + k / 12) * 1000);
        round_corners(&mut frame, PW, PH, PW as f32 * CORNER);
        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&frame, PW as u32, PH as u32);
        let id2 = id.clone();
        let _ = w.upgrade_in_event_loop(move |ui| {
            PREV_PENDING.store(false, Relaxed);
            if PGEN.load(Relaxed) == pgen {
                ui.set_preview_frame(Image::from_rgba8(buf));
                ui.set_preview_time(remaining.into());
                ui.set_preview_id(id2.into());
            }
        });
    }
    // fin del clip (o cancelado): volver a la miniatura
    let _ = w.upgrade_in_event_loop(move |ui| {
        if PGEN.load(Relaxed) == pgen {
            ui.set_preview_id("".into());
        }
    });
}

fn play_thread(sh: Arc<Shared>, gen: u64, id: String, w: Weak<App>, resumed: Option<u64>) {
    let say = |s: String| {
        let _ = w.upgrade_in_event_loop(move |ui| ui.set_status(s.into()));
    };
    let mut resume = resumed;
    for attempt in 0..3 {
        let u = match resolve(&id) {
            Ok(u) => u,
            Err(e) => return say(friendly(&e)),
        };
        let url = audio_url_for(&u);
        if attempt == 0 && resumed.is_none() {
            let (uu, sh2) = (u.clone(), sh.clone());
            let _ = w.upgrade_in_event_loop(move |ui| {
                if sh2.gen.load(Relaxed) == gen {
                    publish_options(&ui, &uu);
                }
            });
        }
        if attempt == 0 && resumed.is_none() && !u.category.is_empty() {
            let (cat, sh2) = (u.category.clone(), sh.clone());
            let _ = w.upgrade_in_event_loop(move |ui| {
                if sh2.gen.load(Relaxed) != gen || MODE_MANUAL.load(Relaxed) {
                    return;
                }
                let topic = STATE.with(|s| {
                    let s = s.borrow();
                    s.cur.and_then(|c| s.queue.get(c)).map(|t| t.artist.ends_with(" - Topic")).unwrap_or(false)
                });
                let want_video = !(is_music_cat(&cat) || topic);
                if ui.get_video_mode() != want_video {
                    ui.set_video_mode(want_video);
                    sync_video(&ui, &sh2);
                }
            });
        }
        audio::trace("URL de stream lista");
        if sh.gen.load(Relaxed) != gen {
            return;
        }
        let buf = Arc::new(audio::Buf::new());
        {
            let (b, s) = (buf.clone(), sh.clone());
            std::thread::spawn(move || audio::download(&url, &b, &s, gen));
        }
        say(String::new());
        if attempt == 0 && resumed.is_none() {
            let (s, i) = (sh.clone(), id.clone());
            std::thread::spawn(move || mark_watched_later(s, gen, i));
        }
        // al cambiar de pista de audio se retoma en el mismo punto
        if let Some(ms) = resume {
            audio::trace(&format!("reanudo en {ms} ms"));
            sh.ring.lock().unwrap().clear();
            sh.base_ms.store(ms, Relaxed);
            sh.played.store(0, Relaxed);
            sh.seek_ms.store(ms as i64, Relaxed);
        }
        match audio::decode(&sh, gen, Box::new(audio::StreamSrc::new(buf))) {
            Ok(true) => {
                let sh2 = sh.clone();
                let _ = w.upgrade_in_event_loop(move |ui| {
                    if sh2.gen.load(Relaxed) == gen {
                        advance(&ui, &sh2, 1);
                    }
                });
                return;
            }
            Ok(false) => return,
            Err(e) => {
                // URL vencida o corte de red: reintentar con una URL nueva retomando donde quedo
                invalidate(&id);
                if attempt == 2 || sh.gen.load(Relaxed) != gen {
                    return say(format!("Reproduccion: {e}"));
                }
                resume = Some(sh.pos_ms());
            }
        }
    }
}

/// URL de audio a reproducir: la pista elegida, o la que eligio yt-dlp (la original).
fn audio_url_for(u: &Urls) -> String {
    let sel = AUDIO_SEL.load(Relaxed);
    if sel != usize::MAX {
        if let Some(o) = u.audio_opts.get(sel) {
            return o.url.clone();
        }
    }
    u.a.clone()
}

/// (URL, etiqueta) del video segun la calidad elegida o, en automatico, la menor que alcance para el tamaño en pantalla.
fn pick_video(u: &Urls, vw: usize, vh: usize) -> Option<(String, String)> {
    let sel = QUALITY_SEL.load(Relaxed);
    if sel >= 0 {
        if let Some(o) = u.video_opts.get(sel as usize) {
            return Some((o.url.clone(), qlabel(o)));
        }
    }
    if !u.video_opts.is_empty() {
        let target = (vh.clamp(360, 2160) as f64 * 0.9) as u32;
        let mut c: Vec<&VideoOpt> = u.video_opts.iter().filter(|o| o.fps <= 30).collect();
        if c.is_empty() {
            c = u.video_opts.iter().collect();
        }
        c.sort_by_key(|o| o.height);
        let o = c.iter().find(|o| o.height >= target).or(c.last())?;
        return Some((o.url.clone(), format!("Automático ({})", qlabel(o))));
    }
    if vw > 640 && !u.v720.is_empty() {
        Some((u.v720.clone(), "Automático (720p)".to_string()))
    } else if !u.v.is_empty() {
        Some((u.v.clone(), "Automático (360p)".to_string()))
    } else {
        None
    }
}

/// Carga en la interfaz las pistas de audio y calidades del video actual.
fn publish_options(ui: &App, u: &Urls) {
    let audio: Vec<SharedString> = u.audio_opts.iter().map(|o| SharedString::from(o.label.as_str())).collect();
    let n = audio.len();
    ui.set_audio_list(ModelRc::new(VecModel::from(audio)));
    let sel = AUDIO_SEL.load(Relaxed);
    let sel_i = if sel == usize::MAX { 0 } else { sel.min(n.saturating_sub(1)) };
    ui.set_audio_sel(sel_i as i32);
    ui.set_audio_label(u.audio_opts.get(sel_i).map(|o| o.label.clone()).unwrap_or_default().into());
    let mut q: Vec<SharedString> = vec![SharedString::from("Automático")];
    q.extend(u.video_opts.iter().map(|o| SharedString::from(qlabel(o).as_str())));
    ui.set_quality_list(ModelRc::new(VecModel::from(q)));
    ui.set_quality_sel(QUALITY_SEL.load(Relaxed) + 1);
}

/// Cambia la pista de audio sin perder el punto de reproduccion.
fn restart_audio(ui: &App, sh: &Arc<Shared>) {
    let id = STATE.with(|s| {
        let s = s.borrow();
        s.cur.and_then(|c| s.queue.get(c)).map(|t| t.id.clone())
    });
    let Some(id) = id else { return };
    let pos = sh.pos_ms();
    let gen = sh.gen.fetch_add(1, Relaxed) + 1;
    sh.ring.lock().unwrap().clear();
    let (s, w) = (sh.clone(), ui.as_weak());
    std::thread::spawn(move || play_thread(s, gen, id, w, Some(pos)));
}

/// Some(true) = musica, Some(false) = video comun, None = todavia no se sabe.
fn music_info(t: &Track) -> Option<bool> {
    if t.music || t.artist.ends_with(" - Topic") {
        return Some(true);
    }
    match META_ST.lock().unwrap().get(&t.id) {
        Some(MetaSt::Done(_, _, m)) => Some(*m),
        _ => None,
    }
}

fn start(ui: &App, sh: &Arc<Shared>, i: usize) {
    let Some(t) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let t = s.queue.get(i).cloned();
        if t.is_some() {
            s.cur = Some(i);
        }
        t
    }) else {
        return;
    };
    let gen = sh.gen.fetch_add(1, Relaxed) + 1;
    sh.ring.lock().unwrap().clear();
    sh.base_ms.store(0, Relaxed);
    sh.played.store(0, Relaxed);
    sh.seek_ms.store(-1, Relaxed);
    sh.paused.store(false, Relaxed);
    ui.set_paused(false);
    AUDIO_SEL.store(usize::MAX, Relaxed);
    QUALITY_SEL.store(-1, Relaxed);
    ui.set_audio_list(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_quality_list(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
    ui.set_audio_label("".into());
    ui.set_quality_label("".into());
    ui.set_settings_open(false);
    if !MODE_MANUAL.load(Relaxed) {
        ui.set_video_mode(music_info(&t).map(|m| !m).unwrap_or(false));
    }
    ui.set_np_title(t.title.clone().into());
    ui.set_np_artist(t.artist.clone().into());
    ui.set_playing_id(t.id.clone().into());
    {
        let base = QBASE.load(Relaxed);
        if i < base || i >= base + 200 {
            publish_queue(ui); // la pista quedo fuera de la ventana mostrada
        } else {
            ui.set_queue_cur((i - base) as i32);
        }
    }
    ui.set_progress(0.0);
    ui.set_np_loaded(false);
    let m = ui.get_items();
    for k in 0..m.row_count() {
        if let Some(r) = m.row_data(k) {
            if r.id.as_str() == t.id && r.loaded {
                ui.set_np_thumb(r.thumb);
                ui.set_np_loaded(true);
            }
        }
    }
    ui.set_np_big_loaded(false);
    ui.set_np_frame_loaded(false);
    *CUES.lock().unwrap() = (String::new(), Vec::new());
    ui.set_cc_available(false);
    ui.set_subtitle_text("".into());
    load_cues(ui.as_weak(), t.id.clone(), ui.get_cc_on());
    ui.set_status("Cargando...".into());
    STATE.with(|s| {
        let s = s.borrow();
        for k in 1..=2 {
            if let Some(n) = s.queue.get(i + k) {
                prefetch(&n.id, k == 1);
            }
        }
    });
    {
        let (w, id) = (ui.as_weak(), t.id.clone());
        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
            if let Some(buf) = fetch_art(&agent, &id) {
                let _ = w.upgrade_in_event_loop(move |ui| {
                    if ui.get_playing_id().as_str() == id {
                        ui.set_np_big(Image::from_rgba8(buf));
                        ui.set_np_big_loaded(true);
                    }
                });
            }
        });
    }
    audio::trace(&format!("CLIC en {}", t.id));
    let (s, w, id) = (sh.clone(), ui.as_weak(), t.id);
    std::thread::spawn(move || play_thread(s, gen, id, w, None));
    sync_video(ui, sh);
}

fn advance(ui: &App, sh: &Arc<Shared>, delta: i32) {
    let next = STATE.with(|s| {
        let s = s.borrow();
        let c = s.cur? as i32 + delta;
        (c >= 0 && (c as usize) < s.queue.len()).then_some(c as usize)
    });
    if let Some(n) = next {
        start(ui, sh, n);
    }
}

fn save_cookies(cs: &[serde_json::Value]) -> usize {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    let mut n = 0;
    for c in cs {
        let d = c["domain"].as_str().unwrap_or("");
        if !yt_domain(d) {
            continue;
        }
        let exp = c["expires"].as_f64().or(c["expirationDate"].as_f64()).unwrap_or(0.0);
        let exp = if exp > 0.0 { exp as i64 } else { 4_102_444_800 };
        let b = |k: &str| if c[k].as_bool().unwrap_or(false) { "TRUE" } else { "FALSE" };
        out += &format!(
            "{d}\t{}\t{}\t{}\t{exp}\t{}\t{}\n",
            if d.starts_with('.') { "TRUE" } else { "FALSE" },
            c["path"].as_str().unwrap_or("/"),
            b("secure"),
            c["name"].as_str().unwrap_or(""),
            c["value"].as_str().unwrap_or("")
        );
        n += 1;
    }
    let _ = std::fs::write(data_dir().join("cookies.txt"), out);
    n
}

fn has_login(cs: &[serde_json::Value]) -> bool {
    let has = |n: &str| cs.iter().any(|c| c["name"] == n && yt_domain(c["domain"].as_str().unwrap_or("")));
    has("LOGIN_INFO") && has("__Secure-3PSID")
}

/// Recibe las cookies que manda la extension de Chrome (solo localhost).
fn handle_sync(mut s: TcpStream, w: &Weak<App>, ext_id: &str) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let (head_end, clen) = loop {
        let Ok(n) = s.read(&mut tmp) else { return };
        if n == 0 || buf.len() > (4 << 20) {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|x| x == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
            let origin = format!("origin: chrome-extension://{ext_id}");
            if !head.starts_with("post /cookies") || !head.contains("x-ytop:") || !head.lines().any(|l| l.trim() == origin) {
                let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                return;
            }
            let clen = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
            break (p + 4, clen);
        }
    };
    while buf.len() < head_end + clen && buf.len() < (4 << 20) {
        match s.read(&mut tmp) {
            Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
            _ => break,
        }
    }
    let body = &buf[head_end.min(buf.len())..buf.len().min(head_end + clen)];
    let saved = serde_json::from_slice::<Vec<serde_json::Value>>(body).ok().filter(|cs| has_login(cs)).map(|cs| save_cookies(&cs));
    let resp: &[u8] = if saved.is_some() {
        b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
    } else {
        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    };
    let _ = s.write_all(resp);
    if saved.is_some() {
        let _ = w.upgrade_in_event_loop(|ui| {
            if ui.get_items().row_count() == 0 || ui.get_status().contains("vencida") {
                STATE.with(|s| s.borrow_mut().cache.clear());
                load_tab(&ui, 0);
            }
        });
    }
}

/// El servidor solo se abre si existe %APPDATA%\ytop\sync_ext_id con el ID (32 letras) de tu extension, y
/// solo acepta pedidos con ese origen: asi otra extension o proceso local no puede cambiar las cookies.
fn start_sync_server(w: Weak<App>) {
    let Some(id) = std::fs::read_to_string(data_dir().join("sync_ext_id"))
        .ok()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| t.len() == 32 && t.chars().all(|c| c.is_ascii_lowercase()))
    else {
        return;
    };
    std::thread::spawn(move || {
        let Ok(l) = TcpListener::bind("127.0.0.1:9334") else { return };
        for s in l.incoming().flatten() {
            handle_sync(s, &w, &id);
        }
    });
}

#[repr(C)]
struct CursorPt {
    x: i32,
    y: i32,
}

/// true si el cursor del sistema se movio desde la ultima consulta.
fn cursor_moved() -> bool {
    let mut p = CursorPt { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    let k = ((p.x as u32 as u64) << 32) | (p.y as u32 as u64);
    LAST_POS.swap(k, Relaxed) != k
}

#[repr(C)]
struct Pmc {
    cb: u32,
    page_faults: u32,
    peak_ws: usize,
    ws: usize,
    _a: usize,
    _b: usize,
    _c: usize,
    _d: usize,
    _pagefile: usize,
    _peak_pagefile: usize,
    private: usize,
}

extern "system" {
    fn SetCursor(h: isize) -> isize;
    fn GetCursorPos(p: *mut CursorPt) -> i32;
    fn GetCurrentProcess() -> isize;
    fn K32GetProcessMemoryInfo(h: isize, p: *mut Pmc, cb: u32) -> i32;
}

/// (working set, memoria privada) en MB del proceso actual.
fn ram_mb() -> Option<(f64, f64)> {
    let mut p = Pmc { cb: std::mem::size_of::<Pmc>() as u32, page_faults: 0, peak_ws: 0, ws: 0, _a: 0, _b: 0, _c: 0, _d: 0, _pagefile: 0, _peak_pagefile: 0, private: 0 };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut p, p.cb) };
    (ok != 0).then(|| (p.ws as f64 / 1048576.0, p.private as f64 / 1048576.0))
}

static TICKS: AtomicU64 = AtomicU64::new(0);

static WAS_MIN: AtomicBool = AtomicBool::new(false);
static REPAINT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Al restaurar la ventana minimizada Windows descarta su contenido y el renderizador por software solo
/// repinta las zonas que cambian: se fuerza un repintado completo durante un instante.
fn repaint_after_restore(ui: &App) {
    let minimized = ui.window().is_minimized();
    if WAS_MIN.swap(minimized, Relaxed) && !minimized {
        REPAINT.store(4, Relaxed);
    }
    let n = REPAINT.load(Relaxed);
    if n > 0 && !minimized {
        REPAINT.store(n - 1, Relaxed);
        ui.set_repaint_flag(!ui.get_repaint_flag());
    }
}

fn tick(ui: &App, sh: &Shared) {
    repaint_after_restore(ui);
    // en modo Video los controles se esconden tras 2.5 s sin mover el mouse (salvo en pausa o con el menu abierto)
    if cursor_moved() {
        LAST_ACT.store(T_ACT.elapsed().as_millis() as u64, Relaxed);
        ui.set_chrome(true);
    }
    if ui.get_video_mode() && ui.get_full() && !ui.get_settings_open() {
        let idle = (T_ACT.elapsed().as_millis() as u64).saturating_sub(LAST_ACT.load(Relaxed));
        if idle > 2500 && !sh.paused.load(Relaxed) {
            ui.set_chrome(false);
        }
    } else {
        ui.set_chrome(true);
    }
    // en pantalla completa, con el mouse quieto el cursor desaparece (reaparece al moverlo)
    if ui.get_cinema() && !ui.get_chrome() {
        unsafe { SetCursor(0) };
    }
    if TICKS.fetch_add(1, Relaxed) % 4 == 0 {
        if let Some((ws, private)) = ram_mb() {
            ui.set_ram_text(format!("RAM {ws:.0} MB · {private:.0} priv").into());
        }
    }
    let Some(dur) = STATE.with(|s| {
        let s = s.borrow();
        s.cur.and_then(|c| s.queue.get(c)).map(|t| t.dur * 1000)
    }) else {
        return;
    };
    let pos = sh.pos_ms().min(dur.max(1));
    ui.set_subtitle_text(if ui.get_cc_on() { cue_at(pos) } else { String::new() }.into());
    ui.set_progress(if dur > 0 { pos as f32 / dur as f32 } else { 0.0 });
    let loading = ui.get_status().as_str() == "Cargando...";
    ui.set_time_text(if loading { format!("0:00 / {}", fmt(dur)).into() } else { format!("{} / {}", fmt(pos), fmt(dur)).into() });
    ui.set_elapsed_text(fmt(pos).into());
    ui.set_remaining_text(format!("-{}", fmt(dur.saturating_sub(pos))).into());
}

fn main() {
    audio::trace("inicio de la app");
    let (sh, mut out, audio_warn) = audio::start_output();
    sh.volume.store(80, Relaxed);
    let ui = App::new().unwrap();
    // aviso de audio (se mantiene aparte del estado mientras no haya una salida abierta)
    let warn_text: String = audio_warn.unwrap_or_else(|| "Sin salida de audio: conectá un dispositivo y se activará solo".to_string());

    {
        let w = ui.as_weak();
        ui.on_select_tab(move |t| {
            if let Some(ui) = w.upgrade() {
                if ui.get_tab() == t && !ui.get_list_mode() {
                    STATE.with(|s| s.borrow_mut().cache.remove(&t));
                    ui.invoke_scroll_top();
                }
                load_tab(&ui, t);
            }
        });
    }
    ui.set_logged(yt_auth().is_some());
    {
        let w = ui.as_weak();
        ui.on_login_paste(move || {
            let Some(ui) = w.upgrade() else { return };
            if ui.get_login_busy() {
                return;
            }
            ui.set_login_busy(true);
            ui.set_login_ok(false);
            ui.set_login_msg("".into());
            let w2 = w.clone();
            std::thread::spawn(move || {
                let res = read_clipboard()
                    .ok_or_else(|| "No pude leer el portapapeles".to_string())
                    .and_then(|t| cookies_to_netscape(&t))
                    .and_then(|(txt, n)| {
                        std::fs::write(data_dir().join("cookies.txt"), txt).map_err(|e| format!("No pude guardar las cookies: {e}"))?;
                        Ok(n)
                    });
                let _ = w2.upgrade_in_event_loop(move |ui| {
                    ui.set_login_busy(false);
                    match res {
                        Ok(n) => {
                            ui.set_login_ok(true);
                            ui.set_login_msg(format!("Listo: {n} cookies guardadas. Recargando tu feed...").into());
                            ui.set_logged(true);
                            STATE.with(|s| s.borrow_mut().cache.clear());
                            let tab = ui.get_tab();
                            load_tab(&ui, if matches!(tab, 0 | 1 | 3 | 4) { tab } else { 0 });
                        }
                        Err(e) => ui.set_login_msg(e.into()),
                    }
                });
            });
        });
    }
    {
        let w = ui.as_weak();
        ui.on_search(move |q| {
            if let Some(ui) = w.upgrade() {
                do_search(&ui, q.to_string(), ui.get_search_kind() == 1);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_play(move |i| {
            let Some(ui) = w.upgrade() else { return };
            stop_preview(Some(&ui));
            MODE_MANUAL.store(false, Relaxed);
            let heading = ui.get_heading().to_string();
            let list_title = ui.get_list_title().to_string();
            let in_list = ui.get_list_mode();
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.queue = s.view.clone();
                s.queue_title = if in_list { list_title } else { heading };
            });
            start(&ui, &sh, i as usize);
            publish_queue(&ui);
            ui.set_full(true);
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_toggle(move || {
            let Some(ui) = w.upgrade() else { return };
            let p = !sh.paused.load(Relaxed);
            sh.paused.store(p, Relaxed);
            ui.set_paused(p);
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_next(move || {
            if let Some(ui) = w.upgrade() {
                advance(&ui, &sh, 1);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_prev(move || {
            if let Some(ui) = w.upgrade() {
                advance(&ui, &sh, -1);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_seek(move |r| {
            let Some(dur) = STATE.with(|s| {
                let s = s.borrow();
                s.cur.and_then(|c| s.queue.get(c)).map(|t| t.dur * 1000)
            }) else {
                return;
            };
            let t = (r.clamp(0.0, 1.0) * dur as f32) as u64;
            sh.base_ms.store(t, Relaxed);
            sh.played.store(0, Relaxed);
            sh.seek_ms.store(t as i64, Relaxed);
            if let Some(ui) = w.upgrade() {
                sync_video(&ui, &sh);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_volume_changed(move |v| {
            sh.volume.store((v * 100.0) as u32, Relaxed);
            if let Some(ui) = w.upgrade() {
                ui.set_volume(v);
            }
        });
    }

    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_video_changed(move |v| {
            MODE_MANUAL.store(true, Relaxed);
            if let Some(ui) = w.upgrade() {
                ui.set_video_mode(v);
                sync_video(&ui, &sh);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_full_changed(move |_| {
            if let Some(ui) = w.upgrade() {
                sync_video(&ui, &sh);
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_hover_track(move |i, on| {
            let g = HOVER_GEN.fetch_add(1, Relaxed) + 1;
            if !on {
                let ui = w.upgrade();
                if let Some(ui) = ui.as_ref() {
                    ui.set_panel_id("".into());
                }
                stop_preview(ui.as_ref());
                return;
            }
            let Some((id, dur, kind)) = STATE.with(|s| s.borrow().view.get(i as usize).map(|t| (t.id.clone(), t.dur, t.kind))) else { return };
            // el panel de color aparece al instante
            if let Some(ui) = w.upgrade() {
                ui.set_panel_id(id.clone().into());
            }
            if kind != 0 {
                return;
            }
            let w = w.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(125));
                if HOVER_GEN.load(Relaxed) != g {
                    return;
                }
                prefetch(&id, true);
                std::thread::sleep(Duration::from_millis(175));
                if HOVER_GEN.load(Relaxed) == g {
                    start_preview(w, id, dur, g);
                }
            });
        });
    }
    {
        let w = ui.as_weak();
        ui.on_more(move || {
            if let Some(ui) = w.upgrade() {
                load_more(&ui);
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_viewport_changed(move || {
            if let Some(ui) = w.upgrade() {
                update_thumbs(&ui);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_cinema_changed(move || {
            if let Some(ui) = w.upgrade() {
                sync_video(&ui, &sh);
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_activity(move || {
            LAST_ACT.store(T_ACT.elapsed().as_millis() as u64, Relaxed);
            if let Some(ui) = w.upgrade() {
                ui.set_chrome(true);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_audio_picked(move |i| {
            let Some(ui) = w.upgrade() else { return };
            AUDIO_SEL.store(i.max(0) as usize, Relaxed);
            ui.set_audio_sel(i);
            if let Some(l) = ui.get_audio_list().row_data(i.max(0) as usize) {
                ui.set_audio_label(l);
            }
            restart_audio(&ui, &sh);
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_quality_picked(move |i| {
            let Some(ui) = w.upgrade() else { return };
            QUALITY_SEL.store(i - 1, Relaxed);
            ui.set_quality_sel(i);
            if i == 0 {
                ui.set_quality_label("Automático".into());
            } else if let Some(l) = ui.get_quality_list().row_data(i as usize) {
                ui.set_quality_label(l);
            }
            sync_video(&ui, &sh);
        });
    }
    {
        let w = ui.as_weak();
        ui.on_open_list(move |i| {
            if let Some(ui) = w.upgrade() {
                stop_preview(Some(&ui));
                open_list(&ui, i.max(0) as usize);
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_list_back(move || {
            if let Some(ui) = w.upgrade() {
                list_back(&ui);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_play_all(move || {
            if let Some(ui) = w.upgrade() {
                start_queue_from_view(&ui, &sh, false);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_shuffle_all(move || {
            if let Some(ui) = w.upgrade() {
                start_queue_from_view(&ui, &sh, true);
            }
        });
    }
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        ui.on_queue_jump(move |i| {
            if let Some(ui) = w.upgrade() {
                start(&ui, &sh, i.max(0) as usize + QBASE.load(Relaxed));
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_start_mix(move || {
            if let Some(ui) = w.upgrade() {
                start_mix(&ui);
            }
        });
    }
    {
        let w = ui.as_weak();
        ui.on_search_kind_changed(move |k| {
            if let Some(ui) = w.upgrade() {
                let q = LAST_Q.lock().unwrap().clone();
                if !q.is_empty() {
                    do_search(&ui, q, k == 1);
                }
            }
        });
    }
    *UIW.lock().unwrap() = Some(ui.as_weak());
    for _ in 0..3 {
        std::thread::spawn(thumb_worker);
    }
    for _ in 0..2 {
        std::thread::spawn(avatar_worker);
    }
    for _ in 0..2 {
        std::thread::spawn(meta_worker);
    }
    std::thread::spawn(prefetch_worker);
    std::thread::spawn(|| {
        let _ = FFMPEG.as_ref();
    });

    {
        let w = ui.as_weak();
        ui.on_cc_changed(move |on| {
            let Some(ui) = w.upgrade() else { return };
            ui.set_cc_on(on);
            if on {
                let id = STATE.with(|s| {
                    let s = s.borrow();
                    s.cur.and_then(|c| s.queue.get(c)).map(|t| t.id.clone())
                });
                if let Some(id) = id {
                    load_cues(w.clone(), id, true);
                }
            } else {
                ui.set_subtitle_text("".into());
            }
        });
    }

    let timer = slint::Timer::default();
    {
        let (w, sh) = (ui.as_weak(), sh.clone());
        timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(ui) = w.upgrade() {
                tick(&ui, &sh);
            }
        });
    }

    // sigue al dispositivo de audio predeterminado de Windows
    ui.set_audio_warn(if out.is_open() { "".into() } else { warn_text.clone().into() });
    let dev_timer = slint::Timer::default();
    {
        let w = ui.as_weak();
        dev_timer.start(TimerMode::Repeated, Duration::from_millis(1000), move || {
            out.check();
            if let Some(ui) = w.upgrade() {
                let msg: &str = if out.is_open() { "" } else { &warn_text };
                if ui.get_audio_warn().as_str() != msg {
                    ui.set_audio_warn(msg.into());
                }
            }
        });
    }

    start_sync_server(ui.as_weak());
    load_tab(&ui, 0);
    ui.run().unwrap();
    sh.gen.fetch_add(1, Relaxed);
    stop_video();
    stop_preview(None);
}
