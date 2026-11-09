# Gear LeverR

[English](README.md) | **Español**

Reescritura en Rust de [Gear Lever](https://github.com/mijorus/gearlever), pensada
para ser más rápida y consumir menos memoria, manteniendo las mismas funciones,
los mismos archivos de configuración y la misma interfaz.

- **UI**: GTK4 + libadwaita + Relm4
- **AppImage**: dos variantes: el build por defecto (usa el GTK del sistema) y
  `gtk-included` (incluye GTK/libadwaita y sus dependencias)
- **Licencia**: GPL-3.0-or-later

## Estado

La versión 0.1 implementa la paridad funcional principal: integración, borrado,
lanzamiento, actualizaciones desde seis fuentes, CLI compatible y la interfaz
completa. Consulta [Limitaciones conocidas](#limitaciones-conocidas).

## Ramas

- `develop`: desarrollo activo. Todos los cambios entran aquí primero.
- `main`: código probado y releases. Se actualiza desde `develop` cuando pasa las pruebas.

## Funciones

- Integrar AppImages con un click, por arrastrar y soltar o desde el selector de archivos.
- Avisar antes de abrir o integrar un AppImage compilado para otra arquitectura de CPU.
- Integrar varios AppImages a la vez y detectar conflictos de nombre (mantener ambos, reemplazar o preguntar en cada uno).
- Extraer icono, nombre, versión y descripción **sin** ejecutar la AppImage y sin
  herramientas externas para SquashFS.
- Gestionar actualizaciones desde URL estática, GitHub, GitLab, Codeberg, Forgejo y FTP,
  incluida la información `.upd_info` embebida (`zsync` y `gh-releases-zsync`).
- Descargar solo los bloques que han cambiado cuando una fuente estática o de GitHub
  publica datos `.zsync`, con descarga completa como respaldo. Este modo delta se puede
  desactivar en Preferencias.
- Actualizar todo a la vez, con progreso, cancelación y comprobación de apps en ejecución.
- Editar argumentos, variables de entorno, sitio web y fuente de actualizaciones.
- Recargar metadata, borrar a la papelera y abrir la carpeta de la app.
- Preferencias, tutorial, about, búsqueda y notificaciones de actualizaciones.
- Token de GitHub opcional en Preferencias, guardado en el llavero del sistema
  con respaldo cifrado, para subir el límite de la API de 60 a 5000 solicitudes
  por hora.
- Traducciones gettext con los catálogos del Gear Lever original (22 idiomas).
- Comprobación de actualizaciones al iniciar sesión mediante el portal
  `org.freedesktop.portal.Background`, con entrada clásica en `~/.config/autostart`
  como respaldo.
- CLI con `--json` compatible con la versión original.

## Requisitos

- GTK 4.8 o superior y libadwaita 1.2 o superior.
- Probado en Debian 12 y Debian 13. Ubuntu 23.04+, Fedora 38+ y Arch incluyen
  versiones compatibles. **Ubuntu 22.04 no** (GTK 4.6 / Adw 1.1).
- Para payloads DwarFS o ISO9660 hacen falta `dwarfsextract`/`7zz`; sin ellos la
  app muestra un error claro.

## Uso

Descarga una de las dos variantes, dale permisos de ejecución y ábrela:

- `GearLeverR-<version>-x86_64.AppImage`: recomendada. Más pequeña y usa el
  GTK 4.8 + libadwaita 1.2 del sistema.
- `GearLeverR-<version>-x86_64-gtk-included.AppImage`: incluye GTK 4 y
  libadwaita dentro del AppImage. Úsala si tu distribución no tiene GTK 4.8 +
  libadwaita 1.2.

```sh
chmod +x GearLeverR-0.1.0-x86_64.AppImage
./GearLeverR-0.1.0-x86_64.AppImage
```

También puedes pasarle un AppImage directamente:

```sh
./GearLeverR-0.1.0-x86_64.AppImage ~/Descargas/App.AppImage
```

### CLI

```sh
./GearLeverR-0.1.0-x86_64.AppImage --integrate App.AppImage
./GearLeverR-0.1.0-x86_64.AppImage --list-installed
./GearLeverR-0.1.0-x86_64.AppImage --list-installed --json
./GearLeverR-0.1.0-x86_64.AppImage --update App.AppImage
./GearLeverR-0.1.0-x86_64.AppImage --update --all
./GearLeverR-0.1.0-x86_64.AppImage --remove App.AppImage
./GearLeverR-0.1.0-x86_64.AppImage --set-update-source App.AppImage \
    --manager GithubUpdater repo=owner/repo repo_filename=App-*.AppImage allow_prereleases=false
./GearLeverR-0.1.0-x86_64.AppImage --list-update-managers
```

Los documentos `--json` tienen `schema_version: 1` y las mismas claves que la
versión original (`installed` / `updates`).

## Registros

Gear LeverR escribe en `~/.local/state/gearlever-r/gearlever-r.log` (o
`$XDG_STATE_HOME/gearlever-r/gearlever-r.log`).

Por defecto solo se registran avisos. Para ver el detalle de las actualizaciones,
activa «Habilitar registros de depuración» en Preferencias → Depurando y
reinicia, o lanza la aplicación desde una terminal con:

```sh
RUST_LOG=gearlever_core=info ./GearLeverR-0.1.0-x86_64.AppImage
```

Después sigue el registro mientras se ejecuta una actualización:

```sh
tail -f ~/.local/state/gearlever-r/gearlever-r.log
```

Las actualizaciones delta solo se intentan con fuentes que publican `.zsync`
(una fuente estática `zsync|...` o un asset `.zsync`/`gh-releases-zsync` de
GitHub). Una actualización delta correcta deja un resumen como:

```text
INFO gearlever_core::update::delta: zsync delta downloaded 87.6 MB of 100.4 MB (12.8 MB saved, 12.8%) downloaded_bytes=91830792 saved_bytes=13451264 total_bytes=105282056 downloaded_blocks=22420 seed_blocks=3284 total_blocks=25704 scan_wait=1.2ms scan=83.1ms download=5.81s
```

- `downloaded_bytes` y `saved_bytes` cuentan bloques verificados; la fusión de
  rangos puede hacer que el tráfico real de red sea algo mayor que
  `downloaded_bytes`.
- `downloaded_blocks`, `seed_blocks` y `total_blocks` son la misma contabilidad
  en bloques.
- `scan_wait` es el tiempo esperando a que termine otro escaneo, `scan` es el
  escaneo en sí y `download` es el tiempo descargando los bloques que faltan.

Si el delta falla, Gear LeverR continúa con la descarga completa y, con los
logs de depuración activados, aparece `zsync delta failed, downloading the full
AppImage`. Usa `--no-zsync-delta` con un comando de actualización para saltar
el delta en esa ejecución sin cambiar Preferencias.

## Compilar

Desarrollo en Debian 13 (o cualquier distro con GTK 4.18/Adw 1.7):

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev \
    libglib2.0-dev liblzma-dev zlib1g-dev
cargo build -p gearlever --release
./target/release/gearlever-r
```

Builds de release (compatibles con Debian 12; glibc 2.36, GTK 4.8, Adw 1.2):

```sh
packaging/appimage/build-in-debian12.sh
```

El script compila dentro de `rust:1.98-bookworm` y empaqueta las dos variantes en
`dist/`: `...AppImage` (por defecto, GTK del sistema) y
`...-gtk-included.AppImage`. La variante `gtk-included` se monta con
`linuxdeploy` y su plugin de GTK, que se descargan automáticamente. Los binarios
resultantes funcionan también en distros más nuevas.

Si ejecutas el binario directamente (sin AppImage), instala el esquema para que
las preferencias persistan:

```sh
install -Dm644 data/io.github.alarez777.GearLeverR.gschema.xml \
    ~/.local/share/glib-2.0/schemas/
glib-compile-schemas ~/.local/share/glib-2.0/schemas
```

Los logs se escriben en `~/.local/state/gearlever-r/gearlever-r.log` y se pueden
abrir desde el menú. El nivel `debug` se activa en Preferencias.

## Traducciones

La interfaz usa gettext y reutiliza los catálogos del Gear Lever original. Los
archivos viven en `po/` (formato `.po`); el AppImage incluye los `.mo` compilados.

```sh
# Compilar para probar en local
scripts/compile-translations.sh
GEARLEVER_R_LOCALEDIR=$PWD/target LANGUAGE=es ./target/release/gearlever-r

# Tras añadir o cambiar textos con t("..."), actualizar plantilla y catálogos
scripts/update-translations.sh
```

Los mensajes nuevos sin traducción muestran el inglés. Para completar un idioma
basta con editar su `.po` (por ejemplo con Poedit) y volver a compilar.

## Pruebas y rendimiento

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace

# Pruebas opcionales con un AppImage real
GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test --workspace

# Benchmarks
scripts/bench-metadata.sh ~/AppImages/*.appimage
scripts/bench-startup.sh

# Humo de UI (Xvfb + openbox + xdotool): sin modal bloqueante y clicks reales
scripts/ui-smoke.sh ./dist/GearLeverR-0.1.0-x86_64.AppImage
```

Resultados medidos en la máquina de desarrollo: consulta
[`docs/performance.md`](docs/performance.md).

## Arquitectura

```
crates/gearlever-core/   lógica sin GTK: appimage, desktop, config, updates, install
crates/gearlever/        binario: CLI (clap) + UI (Relm4)
data/                    .desktop, .gschema, iconos
packaging/appimage/      AppRun, build.sh y build-in-debian12.sh
```

El core no depende de GTK: el CLI funciona sin display y los tests son headless.
La extracción SquashFS usa `backhand` en proceso; no se invocan `7zz`,
`unsquashfs`, `readelf`, `file` ni `ps` en el camino normal.

## Compatibilidad con Gear Lever

- Importa `~/.config/gearlever.conf` la primera vez (incluidas las secciones
  `app.<md5>` con argumentos, variables y fuentes de actualización).
- Importa las claves de GSettings del esquema `it.mijorus.gearlever`.
- Escribe `.desktop` con el mismo formato que el original.
- Usa el mismo directorio `~/AppImages` y `~/AppImages/.icons`.
- Puede convivir con el Flatpak instalado.

## Limitaciones conocidas

- La interfaz está traducida: se reutilizan los catálogos del original. El español
  está completo; en otros idiomas, las cadenas nuevas de Gear LeverR pueden
  aparecer en inglés hasta que se traduzcan (ver `po/`).
- El tutorial tiene tres páginas (el original tiene cuatro) y no incluye el
  AppImage de demostración.
- Las variables de entorno se editan como líneas `KEY=value`, no fila por fila.
- DwarFS e ISO9660 requieren `dwarfsextract` o `7zz` instalados.
- No hay auto-actualización del propio AppImage.

## Licencia y atribución

GPL-3.0-or-later. Es un trabajo derivado de Gear Lever, de Lorenzo Paderi
(<https://github.com/mijorus/gearlever>). El nombre y el icono originales no se
usan; este proyecto se distribuye como Gear LeverR.

Las actualizaciones delta usan [zsync-rs](https://crates.io/crates/zsync-rs), con
licencia MIT. Su aviso está en `LICENSES/zsync-rs-MIT.txt` y dentro del AppImage.
