# AGENTS.md — Gear Lever R

## Identidad del proyecto

Reescritura en Rust de [Gear Lever](https://github.com/mijorus/gearlever) (GPL-3.0-or-later).
El original está clonado en `/home/alarez/Proyectos/gearlever`; es **solo referencia de
comportamiento**: no se modifica. Ese directorio tiene su propio `AGENTS.md`.

| Campo | Valor |
| --- | --- |
| Nombre | Gear Lever R |
| App ID | `io.github.alarez777.GearLeverR` |
| Binario | `gearlever-r` |
| AppImage | `GearLeverR-<version>-x86_64.AppImage` |
| Config | `~/.config/gearlever-r.conf` |
| Licencia | GPL-3.0-or-later (obra derivada; conservar atribución a Lorenzo Paderi) |

## Skills obligatorias: Rust (BeautifulRust)

Instaladas en `~/.config/opencode/skills/`. **Usarlas siempre**, sin esperar a que se pidan:

| Skill | Cuándo |
| --- | --- |
| `rust-best-practices` | Antes de escribir o refactorizar cualquier código Rust. |
| `rust-review` | Al revisar código, un `.rs`, una crate, un diff o un PR. |
| `rust-port` | Al portar código desde C, C++, Python, Go, JS o TS. |

Las reglas de estilo y API se consultan en la skill, no se improvisan.

## Stack

- Rust edition 2024, `rust-version = "1.93"` (lo exige Relm4 0.11).
- UI: **GTK4 + libadwaita + Relm4 0.11** (`features = ["libadwaita"]`).
  - Se compila contra GTK 4.18 / Adw 1.7 (Debian 13) pero **la API usada debe existir en
    GTK 4.8 / libadwaita 1.2** para soportar Debian 12 y Ubuntu 23.04+.
  - Prohibido por ahora: `AdwToolbarView`, `AdwNavigationView`, `AdwBanner`,
    `AdwToggleGroup`, `AdwAlertDialog` (son 1.3/1.4/1.5). Usar equivalentes propios.
- Async: `tokio`; HTTP: `reqwest` con **rustls** (nunca OpenSSL, para no romper el AppImage).
- CLI: `clap` (derive). Errores: `thiserror` en la librería, `anyhow` solo en el binario.
- SquashFS: `backhand` (sin binarios externos). ISO9660: `iso9660`. FTP: `suppaftp`.
- Sin `unwrap()`/`expect()` en rutas de producción; `expect("motivo")` solo para invariantes.

**Builds de release:** el AppImage se compila en un contenedor **Debian 12** (glibc 2.36,
GTK 4.8, Adw 1.2) para que siga funcionando en Debian 12 aunque el desarrollo se haga en
Debian 13. Los binarios locales de Debian 13 solo sirven para probar. Usar
`packaging/appimage/build-in-debian12.sh` y no compilar el AppImage en el host.

## Estructura

```
crates/gearlever-core/   lógica sin GTK: appimage, desktop, config, updates, install
crates/gearlever/        binario: CLI (clap) + UI (Relm4)
data/                    .desktop, metainfo, gschema, iconos
po/                      traducciones heredadas del original
packaging/appimage/      AppRun, receta y CI del AppImage
tests/                   integración CLI y fixtures
```

El core **no** depende de GTK: así el CLI funciona sin display y los tests son headless.

## Compatibilidad con Gear Lever (obligatoria)

- Leer `~/.config/gearlever.conf` (secciones `app.<md5(ruta)>` y `app.<md5>.update_manager`)
  al importar; a partir de ahí escribir `gearlever-r.conf`.
- Escribir `.desktop` con el mismo formato: `TryExec`, `Exec`, `Icon`, `Actions`,
  `X-AppImage-Version`, `X-AppImage-Name`, `DESKTOPINTEGRATION=1`.
- Mismos IDs de gestor de updates: `StaticFileUpdater`, `GithubUpdater`, `GitlabUpdater`,
  `CodebergUpdater`, `FTPUpdater`, `ForgejoUpdater`.
- CLI con el mismo comportamiento y `--json` con `schema_version: 1`.
- Directorio de AppImages y `.icons` sin cambios (`~/AppImages` por defecto).

## Calidad antes de entregar

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

- Lints en `[workspace.lints]` (copiadas de BeautifulRust) y `clippy.toml` en la raíz.
- Suprimir con `#[expect(lint, reason = "...")]`, nunca `#[allow]` sin motivo.
- Tests junto al código o en `tests/`; los que tocan red/tiempo deben ser mockeables.

## Convenciones

- Español para hablar con el usuario; inglés para identificadores, docs y commits.
- Commits pequeños y revisables; nada de refactors masivos no pedidos.
- No agregar funcionalidades nuevas: el objetivo es paridad + rendimiento.
- Medir antes de optimizar; los benchmarks viven en `benches/` o `scripts/`.
