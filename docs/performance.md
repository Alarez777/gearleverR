# Rendimiento

Medido en la máquina de desarrollo (Debian 13, 32 núcleos, Wayland, mismos
AppImages y mismos 68 archivos `.desktop`).

## Línea base: Gear Lever 4.6.2 (Flatpak)

| Métrica | Valor |
| --- | --- |
| `--list-installed` (arranque completo) | 241-253 ms |
| Arranque del GUI hasta registrar el bus | ~200 ms |
| PSS en reposo (GUI) | ~123 MB |
| Memoria privada (GUI) | ~124 MB |
| Metadata de una app | md5 del AppImage completo + subproceso `7zz`/`unsquashfs`/`dwarfsck` |

## Gear Lever R 0.1

| Métrica | Valor |
| --- | --- |
| `--list-installed` (binario directo) | **11-15 ms** |
| `--list-installed` (AppImage, incluye montaje) | **21-24 ms** |
| Arranque del GUI hasta registrar el bus | **41-47 ms** |
| PSS en reposo (lista vacía) | **~59 MB** |
| PSS con página de detalles | **~82 MB** |
| Memoria privada (lista vacía) | **~41 MB** |
| Metadata de 38 AppImages reales | **0.1-23 ms** por app, sin subprocesos |

## Cómo se mide

```sh
# CLI: tres ejecuciones, con /usr/bin/time o midiendo el reloj
scripts/bench-metadata.sh ~/AppImages/*.appimage

# GUI: tiempo hasta que la app posee su nombre en D-Bus y PSS tras 3 s
scripts/bench-startup.sh
```

Las cifras de memoria usan `Pss` de `/proc/<pid>/smaps_rollup`. La comparación
con el Flatpak incluye la copia privada de GTK que trae el runtime; la versión
nativa comparte GTK con el resto del sistema, por eso su memoria privada es la
cifra relevante.

## De dónde sale la mejora

- Sin intérprete Python ni PyGObject al arrancar.
- Extracción SquashFS en proceso con `backhand` (zstd/xz/lz4/gzip).
- Caché de metadata e iconos invalidada por tamaño y mtime, en lugar de hashear
  el AppImage completo en cada vista.
- Un solo pase de hashing, solo cuando el usuario lo pide.
- Comprobación de actualizaciones en paralelo (8 en vuelo) y caché de SHA-1.
- Detección de procesos vía `/proc` y arquitectura vía cabecera ELF.
- Cero subprocesos en el flujo normal de listar, integrar y detalles.
