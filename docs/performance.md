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

## Gear LeverR 0.1

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

## Actualizaciones delta (zsync)

Medido con un AppImage real de 115.210.744 bytes, bloques de 4096 bytes y un
servidor HTTP local, en la máquina de desarrollo (32 núcleos, release):

| Cambios en la versión nueva | Bytes descargados | Tiempo total | Escaneo | Rangos |
| --- | --- | --- | --- | --- |
| 1 bloque de 4 KB | 4 KB (0,004 %) | 131 ms | 83 ms | 1 petición |
| 10 % de bloques, repartidos | 11,6 MB (10,04 %) | 5,95 s | 91 ms | 2.813 peticiones |
| 50 % de bloques, repartidos | 115 MB (100 %) | 266 ms | 100 ms | rangos fusionados |

El escaneo del AppImage instalado cuesta unos 80-100 ms en esta máquina; en
equipos modestos será bastante mayor, que es el motivo del aviso de Preferencias.
El caso de un solo bloque es el que más se beneficia. Cuando los cambios están
repartidos cada pocos bloques, `zsync-rs` abre muchos rangos pequeños y el
tiempo de red puede superar al de una descarga completa; con cambios muy densos
los rangos se fusionan y se acaba descargando el archivo entero. Por eso el
ajuste queda activado por defecto y se puede desactivar sin cambiar de fuente.

## De dónde sale la mejora

- Sin intérprete Python ni PyGObject al arrancar.
- Extracción SquashFS en proceso con `backhand` (zstd/xz/lz4/gzip).
- Caché de metadata e iconos invalidada por tamaño y mtime, en lugar de hashear
  el AppImage completo en cada vista.
- Un solo pase de hashing, solo cuando el usuario lo pide.
- Comprobación de actualizaciones en paralelo (5 en vuelo, también en la CLI) y caché de SHA-1.
- Caché HTTP condicional (ETag/`304`) con deduplicación de peticiones concurrentes.
- Descargas de un lote con 2-3 preparaciones en paralelo e integración serial; sin copia
  intermedia del AppImage.
- Actualizaciones delta con `zsync-rs` para las fuentes que publican `.zsync`: solo se
  descargan los bloques que faltan y el escaneo del AppImage instalado se serializa.
- Detección de procesos vía `/proc` y arquitectura vía cabecera ELF.
- Cero subprocesos en el flujo normal de listar, integrar y detalles.
