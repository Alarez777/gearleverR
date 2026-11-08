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

El control `.zsync` se descarga una sola vez por comprobación y se reutiliza en
la descarga delta, en lugar de pedirlo otra vez a `zsync-rs`.

El escaneo del AppImage instalado cuesta unos 80-100 ms en esta máquina; en
equipos modestos será bastante mayor, que es el motivo del aviso de Preferencias.
El caso de un solo bloque es el que más se beneficia. Cuando los cambios están
repartidos cada pocos bloques, `zsync-rs` abre muchos rangos pequeños y el
tiempo de red puede superar al de una descarga completa; con cambios muy densos
los rangos se fusionan y se acaba descargando el archivo entero. Por eso el
ajuste queda activado por defecto y se puede desactivar sin cambiar de fuente.

Prueba real con Neovide 0.16.2 → 0.17.2 (12,9 MB): solo se reutilizaron 198 de
6.582 bloques y se descargaron 12,5 MB. El payload SquashFS se había recompreso
casi por completo, así que el delta apenas ahorró bytes en ese caso. El resumen
aparece en el log como `zsync delta downloaded 12.5 MB of 12.9 MB (396.0 KB
saved, 3.0%)`.

En tres actualizaciones reales más, de entre 90 y 118 MB, el resumen fue:

| Descargado | Tamaño final | Ahorro | Bloques reutilizados |
| ---: | ---: | ---: | ---: |
| 87,8 MB | 90,9 MB | 3,0 MB (3,3 %) | 1.557 / 46.526 |
| 87,6 MB | 100,4 MB | 12,8 MB (12,8 %) | 3.284 / 25.704 |
| 115,2 MB | 118,3 MB | 3,1 MB (2,6 %) | 800 / 30.294 |

Dos de esos casos recompusieron casi todo el payload y ahorraron alrededor del
3 %; el otro ahorró un 12,8 %. El delta se usó en los tres, pero el beneficio
depende de cuánto se parezca la versión instalada a la nueva. El ahorro real de
red puede ser algo menor que el del log, porque la fusión de rangos puede
volver a descargar pequeños huecos entre bloques.

### Prueba con dos núcleos

Misma actualización real de Neovide 0.16.2 → 0.17.2, limitando el proceso con
`taskset -c 0,1` para simular una máquina modesta (el disco no se limitó):

| Núcleos | Descargado | Escaneo | Descarga |
| --- | ---: | ---: | ---: |
| 32 | 12,5 de 12,9 MB | 80,6 ms | 559 ms |
| 2 | 12,5 de 12,9 MB | 224,9 ms | 400 ms |

El escaneo se multiplica por ~2,8 al pasar de 32 a 2 núcleos, como corresponde
a un trabajo CPU-bound. La descarga varía por la red y no es una medida
fiable de disco; para eso haría falta un benchmark con la caché de disco fría.
El resultado respalda mantener el aviso de Preferencias para equipos de muy
bajos recursos.

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
