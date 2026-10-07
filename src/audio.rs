use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

static T0: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);

/// Registro de tiempos para diagnostico (solo si existe la variable YTOP_TRACE).
pub fn trace(msg: &str) {
    if std::env::var_os("YTOP_TRACE").is_none() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("ytop_trace.log")) {
        let _ = writeln!(f, "{:>8.2}s {msg}", T0.elapsed().as_secs_f64());
    }
}

pub struct Shared {
    pub ring: Mutex<VecDeque<f32>>,
    pub gen: AtomicU64,
    pub paused: AtomicBool,
    pub volume: AtomicU32, // 0..=100
    pub played: AtomicU64, // frames reproducidos desde `base_ms`
    pub base_ms: AtomicU64,
    pub seek_ms: AtomicI64, // -1 = sin pedido
    pub rate: u32,
    pub channels: usize,
}

impl Shared {
    pub fn pos_ms(&self) -> u64 {
        self.base_ms.load(Relaxed) + self.played.load(Relaxed) * 1000 / self.rate as u64
    }
}

/// Salida de audio: sigue al dispositivo predeterminado de Windows. El `Stream` debe mantenerse vivo.
pub struct Output {
    stream: Option<cpal::Stream>,
    name: String,
    sh: Arc<Shared>,
    failed: Arc<AtomicBool>,
    last_try: std::time::Instant,
}

/// Estado del conversor de frecuencia dentro del callback de salida.
struct OutState {
    pos: f64,
    a: Vec<f32>,
    b: Vec<f32>,
}

/// Crea el stream en `dev`. El ring tiene siempre el formato inicial (`sh.rate` / `sh.channels`);
/// si el dispositivo usa otra frecuencia o cantidad de canales, el callback convierte.
fn build_stream(dev: &cpal::Device, sh: &Arc<Shared>, failed: &Arc<AtomicBool>) -> Result<cpal::Stream, String> {
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    if cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err("formato de salida no soportado (se esperaba f32)".into());
    }
    let (ring_ch, dev_ch) = (sh.channels, cfg.channels() as usize);
    let ratio = sh.rate as f64 / cfg.sample_rate().0 as f64;
    let direct = ring_ch == dev_ch && (ratio - 1.0).abs() < 1e-9;
    let s = sh.clone();
    let f = failed.clone();
    let mut st = OutState { pos: 1.0, a: vec![0.0; ring_ch], b: vec![0.0; ring_ch] };
    let stream = dev
        .build_output_stream(
            &cfg.into(),
            move |out: &mut [f32], _| {
                if s.paused.load(Relaxed) {
                    out.fill(0.0);
                    return;
                }
                let vol = s.volume.load(Relaxed) as f32 / 100.0;
                let vol = vol * vol;
                let mut r = s.ring.lock().unwrap();
                let mut frames = 0usize;
                if direct {
                    let mut n = 0;
                    for o in out.iter_mut() {
                        match r.pop_front() {
                            Some(v) => {
                                *o = v * vol;
                                n += 1;
                            }
                            None => *o = 0.0,
                        }
                    }
                    frames = n / ring_ch;
                } else {
                    for frame in out.chunks_mut(dev_ch) {
                        while st.pos >= 1.0 {
                            st.pos -= 1.0;
                            std::mem::swap(&mut st.a, &mut st.b);
                            if r.len() >= ring_ch {
                                for c in 0..ring_ch {
                                    st.b[c] = r.pop_front().unwrap_or(0.0);
                                }
                                frames += 1;
                            } else {
                                st.b.fill(0.0);
                            }
                        }
                        let t = st.pos as f32;
                        for (oc, o) in frame.iter_mut().enumerate() {
                            let sc = if ring_ch == 1 { Some(0) } else if oc < ring_ch { Some(oc) } else { None };
                            *o = match sc {
                                Some(c) => (st.a[c] + (st.b[c] - st.a[c]) * t) * vol,
                                None => 0.0,
                            };
                        }
                        st.pos += ratio;
                    }
                }
                s.played.fetch_add(frames as u64, Relaxed);
            },
            move |_| f.store(true, Relaxed),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn default_name(dev: &Option<cpal::Device>) -> String {
    dev.as_ref().and_then(|d| d.name().ok()).unwrap_or_default()
}

/// Abre la salida por defecto.
pub fn start_output() -> Result<(Arc<Shared>, Output), String> {
    let dev = cpal::default_host().default_output_device();
    let name = default_name(&dev);
    let dev = dev.ok_or("sin dispositivo de audio")?;
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let sh = Arc::new(Shared {
        ring: Mutex::new(VecDeque::with_capacity(96_000 * 2)),
        gen: AtomicU64::new(0),
        paused: AtomicBool::new(false),
        volume: AtomicU32::new(80),
        played: AtomicU64::new(0),
        base_ms: AtomicU64::new(0),
        seek_ms: AtomicI64::new(-1),
        rate: cfg.sample_rate().0,
        channels: cfg.channels() as usize,
    });
    let failed = Arc::new(AtomicBool::new(false));
    let stream = build_stream(&dev, &sh, &failed)?;
    trace(&format!("audio: salida '{name}'"));
    Ok((sh.clone(), Output { stream: Some(stream), name, sh, failed, last_try: std::time::Instant::now() }))
}

impl Output {
    /// Si Windows cambio el dispositivo predeterminado (o el actual fallo), pasa el sonido al nuevo.
    pub fn check(&mut self) {
        let dev = cpal::default_host().default_output_device();
        let name = default_name(&dev);
        let failed = self.failed.load(Relaxed);
        if name.is_empty() || (name == self.name && !failed) {
            return;
        }
        if self.last_try.elapsed() < std::time::Duration::from_millis(900) {
            return;
        }
        self.last_try = std::time::Instant::now();
        let Some(dev) = dev else { return };
        match build_stream(&dev, &self.sh, &self.failed) {
            Ok(st) => {
                self.failed.store(false, Relaxed);
                self.stream = Some(st);
                trace(&format!("audio: cambio de salida '{}' -> '{}'", self.name, name));
                self.name = name;
            }
            Err(e) => trace(&format!("audio: no se pudo abrir '{name}': {e}")),
        }
    }
}

struct Resampler {
    ratio: f64,
    frac: f64,
    prev: Vec<f32>,
    ch: usize,
}

impl Resampler {
    fn new(src: u32, dst: u32, ch: usize) -> Self {
        Self { ratio: src as f64 / dst as f64, frac: 0.0, prev: vec![0.0; ch], ch }
    }
    fn reset(&mut self) {
        self.frac = 0.0;
        self.prev.fill(0.0);
    }
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.ch;
        let n = input.len() / ch;
        if n == 0 {
            return;
        }
        if (self.ratio - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        let mut t = self.frac;
        while t < n as f64 {
            let i = t as usize;
            let f = (t - i as f64) as f32;
            for c in 0..ch {
                let a = if i == 0 { self.prev[c] } else { input[(i - 1) * ch + c] };
                let b = input[i * ch + c];
                out.push(a + (b - a) * f);
            }
            t += self.ratio;
        }
        self.frac = t - n as f64;
        self.prev.copy_from_slice(&input[(n - 1) * ch..n * ch]);
    }
}

/// Decodifica un m4a/AAC en memoria y lo vuelca al ring buffer.
/// Devuelve Ok(true) si terminó naturalmente, Ok(false) si fue cancelado.
pub fn decode(sh: &Shared, gen: u64, src: Box<dyn MediaSource>) -> Result<bool, String> {
    let mss = MediaSourceStream::new(src, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("sin pista de audio")?;
    let tid = track.id;
    let params = track.codec_params.clone();
    let mut dec = symphonia::default::get_codecs()
        .make(&params, &Default::default())
        .map_err(|e| e.to_string())?;
    let src_rate = params.sample_rate.unwrap_or(44100);
    let src_ch = params.channels.map(|c| c.count()).unwrap_or(2).max(1);
    let mut rs = Resampler::new(src_rate, sh.rate, src_ch);
    let oc = sh.channels;
    let limit = sh.rate as usize * oc; // ~1 s de audio en cola
    let mut first = true;
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    let mut tmp: Vec<f32> = Vec::new();

    loop {
        if sh.gen.load(Relaxed) != gen {
            return Ok(false);
        }
        let sk = sh.seek_ms.swap(-1, Relaxed);
        if sk >= 0 {
            let t = Time::new((sk / 1000) as u64, (sk % 1000) as f64 / 1000.0);
            let _ = format.seek(SeekMode::Coarse, SeekTo::Time { time: t, track_id: Some(tid) });
            dec.reset();
            rs.reset();
            sh.ring.lock().unwrap().clear();
        }
        while sh.ring.lock().unwrap().len() > limit {
            if sh.gen.load(Relaxed) != gen || sh.seek_ms.load(Relaxed) >= 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let pkt = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };
        if pkt.track_id() != tid {
            continue;
        }
        let buf = match dec.decode(&pkt) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let sb = sbuf.get_or_insert_with(|| SampleBuffer::new(buf.capacity() as u64, *buf.spec()));
        sb.copy_interleaved_ref(buf);
        tmp.clear();
        rs.process(sb.samples(), &mut tmp);
        if first {
            first = false;
            trace("primer audio decodificado");
        }
        let mut ring = sh.ring.lock().unwrap();
        for fr in tmp.chunks_exact(src_ch) {
            for c in 0..oc {
                ring.push_back(if src_ch == 1 { fr[0] } else { fr[c % src_ch] });
            }
        }
    }
    while !sh.ring.lock().unwrap().is_empty() {
        if sh.gen.load(Relaxed) != gen {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(true)
}

/// Buffer compartido que se va llenando mientras se descarga el audio.
pub struct Buf {
    data: Mutex<Vec<u8>>,
    cv: Condvar,
    done: AtomicBool,
    failed: AtomicBool,
    total: AtomicU64,
}

impl Buf {
    pub fn new() -> Self {
        Self { data: Mutex::new(Vec::new()), cv: Condvar::new(), done: AtomicBool::new(false), failed: AtomicBool::new(false), total: AtomicU64::new(0) }
    }
}

/// Descarga `url` al buffer en bloques; se cancela si cambia la pista.
pub fn download(url: &str, buf: &Buf, sh: &Shared, gen: u64) {
    let fail = || {
        buf.failed.store(true, Relaxed);
        buf.cv.notify_all();
    };
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(20)).build();
    let resp = match agent.get(url).call() {
        Ok(r) => r,
        Err(_) => return fail(),
    };
    if let Some(n) = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()) {
        buf.total.store(n, Relaxed);
        buf.data.lock().unwrap().reserve(n as usize);
    }
    let mut r = resp.into_reader();
    let mut chunk = [0u8; 32768];
    loop {
        if sh.gen.load(Relaxed) != gen {
            return fail();
        }
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.data.lock().unwrap().is_empty() {
                    trace("descarga: primeros bytes");
                }
                buf.data.lock().unwrap().extend_from_slice(&chunk[..n]);
                buf.cv.notify_all();
            }
            Err(_) => return fail(),
        }
    }
    buf.done.store(true, Relaxed);
    buf.cv.notify_all();
}

/// Fuente de medios que lee del buffer mientras todavia se esta descargando.
pub struct StreamSrc {
    buf: Arc<Buf>,
    pos: u64,
}

impl StreamSrc {
    pub fn new(buf: Arc<Buf>) -> Self {
        Self { buf, pos: 0 }
    }
}

fn io_err(m: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, m.to_string())
}

impl Read for StreamSrc {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut g = self.buf.data.lock().unwrap();
        loop {
            let len = g.len() as u64;
            if self.pos < len {
                let n = out.len().min((len - self.pos) as usize);
                out[..n].copy_from_slice(&g[self.pos as usize..self.pos as usize + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if self.buf.done.load(Relaxed) {
                return Ok(0);
            }
            if self.buf.failed.load(Relaxed) {
                return Err(io_err("descarga interrumpida"));
            }
            g = self.buf.cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
        }
    }
}

impl Seek for StreamSrc {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        let np: i64 = match p {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => {
                let mut total = self.buf.total.load(Relaxed);
                if total == 0 {
                    // sin Content-Length: esperar a tener todo
                    let mut g = self.buf.data.lock().unwrap();
                    while !self.buf.done.load(Relaxed) {
                        if self.buf.failed.load(Relaxed) {
                            return Err(io_err("descarga interrumpida"));
                        }
                        g = self.buf.cv.wait_timeout(g, Duration::from_millis(200)).unwrap().0;
                    }
                    total = g.len() as u64;
                }
                total as i64 + d
            }
        };
        if np < 0 {
            return Err(io_err("posicion negativa"));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

impl MediaSource for StreamSrc {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        let t = self.buf.total.load(Relaxed);
        (t > 0).then_some(t)
    }
}
