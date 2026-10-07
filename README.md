# YT Op

Reproductor de YouTube / YouTube Music liviano para Windows, escrito en Rust con [Slint](https://slint.dev).
Apunta a consumir poca RAM (unos 30 MB en reposo) y funciona aunque `music.youtube.com` esté bloqueado, porque usa `www.youtube.com` a través de `yt-dlp`.

## Qué tiene

- Feed (Inicio, Me gusta, Historial, Listas) con tarjetas al estilo de YouTube, vista previa al pasar el mouse y búsqueda.
- Reproductor con modo Audio y modo Video, pantalla completa, subtítulos, calidad y pista de audio seleccionables.
- Listas de reproducción y mixes, con cola de reproducción, reproducir todo y aleatorio.
- La salida de audio sigue al dispositivo predeterminado de Windows.
- Medidor de RAM en la cabecera.

## Requisitos

- Windows 10/11
- [Rust](https://rustup.rs) (para compilar)
- `yt-dlp` en el `PATH`
- `ffmpeg` (se usa para decodificar el video; la app lo busca automáticamente)

## Compilar y ejecutar

```
cargo build --release
target\release\ytop.exe
```

## Iniciar sesión

La app usa las cookies de tu sesión de YouTube para el feed, los "Me gusta" y el historial.
Tocá **Iniciar sesión**, copiá las cookies desde el navegador (extensión Cookie-Editor → Export, o el encabezado `Cookie` de las herramientas de desarrollador) y tocá **Pegar y guardar**.
Se guardan en `%APPDATA%\ytop\cookies.txt`. Ese archivo contiene credenciales: no lo compartas ni lo subas a ningún repositorio.

## Estructura

- `src/main.rs`: lógica de la app (feed, listas, reproducción, miniaturas)
- `src/audio.rs`: salida de audio y decodificación
- `ui/app.slint`: interfaz
- `extension/`: extensión opcional para sincronizar cookies
