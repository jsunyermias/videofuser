# Especificación del sistema videofuser

**Versión**: 1.0
**Estado**: Cerrada, lista para implementación
**Fecha**: 6 de mayo de 2026
**Lenguaje principal de implementación**: Rust
**Plataforma objetivo**: Linux (FUSE)

---

## Tabla de contenidos

1. [Resumen ejecutivo](#1-resumen-ejecutivo)
2. [Glosario y terminología](#2-glosario-y-terminología)
3. [Visión general del sistema](#3-visión-general-del-sistema)
4. [Arquitectura](#4-arquitectura)
5. [Lado publicador: pipeline](#5-lado-publicador-pipeline)
6. [Estructura del torrent](#6-estructura-del-torrent)
7. [Formato del binstruct](#7-formato-del-binstruct)
8. [Formato de los archivos VFR](#8-formato-de-los-archivos-vfr)
9. [Formatos del manifest](#9-formatos-del-manifest)
10. [Los parsers](#10-los-parsers)
11. [El demonio videofuser](#11-el-demonio-videofuser)
12. [Comportamiento en runtime](#12-comportamiento-en-runtime)
13. [El muxer](#13-el-muxer)
14. [Concurrencia y estado](#14-concurrencia-y-estado)
15. [Algoritmos clave](#15-algoritmos-clave)
16. [Casos límite y políticas](#16-casos-límite-y-políticas)
17. [Binarios y crates del workspace](#17-binarios-y-crates-del-workspace)
18. [Protocolo IPC](#18-protocolo-ipc)
19. [Roadmap de implementación](#19-roadmap-de-implementación)
20. [Apéndices](#20-apéndices)

---

## 1. Resumen ejecutivo

videofuser es un sistema que permite distribuir mediante BitTorrent un archivo Matroska (MKV) con múltiples pistas de vídeo en distintas resoluciones, múltiples pistas de audio en distintos códecs e idiomas y múltiples pistas de subtítulos en distintos formatos e idiomas, de manera que cada receptor descargue **solo las pistas que le interesan**, pueda **cambiar de selección antes, durante o después de la reproducción**, y disponga en todo momento de un **MKV virtual reproducible** que se va completando a medida que llegan los datos.

El sistema se basa en una idea central: el torrent no contiene un archivo MKV. Contiene los archivos de cada pista por separado más un archivo binstruct que codifica la estructura necesaria para reconstruir un MKV virtual equivalente al original. En el lado del receptor, un demonio único monta un sistema de archivos FUSE que sirve el MKV virtual reconstruyéndolo on-the-fly desde el binstruct y los archivos crudos descargados, rellenando con ceros las regiones aún no disponibles.

El MKV virtual servido al usuario está **filtrado según sus preferencias**: solo incluye las pistas de audio en los idiomas que el usuario quiere ver (más el idioma original del contenido) y solo expone los sidecars de subtítulos en los idiomas que el usuario quiere leer. Esto reduce el número de pistas visibles a un rango cómodo (típicamente 15-30) muy por debajo de las recomendaciones del spec de Matroska (127) y de los límites prácticos de los reproductores.

El sistema se compone de dos lados claramente separados:

- **Lado publicador**: un conjunto de binarios y herramientas que toman un MKV completo, generan reescalados de la pista de vídeo, extraen todas las pistas a archivos crudos, generan los índices necesarios y empaquetan el resultado en una estructura de directorios apta para ser distribuida por torrent.
- **Lado receptor**: un demonio único llamado `videofuser` que vigila los torrents añadidos al cliente del usuario, reconoce los que tienen la estructura del sistema, monta para cada uno un MKV virtual y los subtítulos asociados bajo un punto de montaje FUSE compartido, y aplica las preferencias del usuario para seleccionar qué pistas se incluyen y cuáles se descargan automáticamente.

El sistema está diseñado para ser **modular** (parsers de códec independientes, separación core/parsers), **eficiente** (binstruct ligero, carga perezosa, mmap), **robusto** (bloqueo indefinido por defecto en lugar de devolver datos corruptos) y **extensible** (formato EBML propio versionable, schema versionado, capacidad de añadir parsers nuevos sin tocar el core).

---

## 2. Glosario y terminología

A lo largo del documento se usan los siguientes términos con el significado preciso aquí indicado:

- **MKV original**: archivo Matroska de partida que el publicador desea distribuir.
- **MKV intermedio**: archivo Matroska generado por el publicador en el paso de re-mux, que contiene las 8 pistas de vídeo y todas las pistas de audio del original, pero **sin pistas de subtítulos**. Debe tener exactamente una pista de audio con `FlagDefault=1`.
- **MKV virtual**: archivo Matroska sintético servido por el demonio `videofuser` a través de FUSE, reconstruido a partir del binstruct y los archivos crudos. Su Tracks element contiene un subconjunto de las pistas del MKV intermedio según las preferencias del usuario.
- **Pista** (track): una corriente individual de datos dentro de un MKV (vídeo, audio o subtítulo).
- **Track ID**: identificador único de una pista dentro del MKV original. Numérico, asignado por el publicador y consistente en toda la estructura del torrent.
- **Block / SimpleBlock**: unidad de payload dentro de un Cluster en formato Matroska, que contiene uno o varios frames de una pista junto con una cabecera de metadatos.
- **Cluster**: contenedor EBML dentro de un MKV que agrupa Blocks de varias pistas en una ventana temporal común.
- **Frame**: unidad atómica de datos de un códec (un cuadro de vídeo, un frame de audio, una línea de subtítulo).
- **Lacing**: técnica de Matroska para empaquetar varios frames pequeños dentro de un único Block. Eliminado en este sistema (cada frame se emite en su propio SimpleBlock).
- **EBML**: Extensible Binary Meta Language, formato base de Matroska, también usado como formato propio del binstruct.
- **CBR / VBR**: Constant Bit Rate / Variable Bit Rate. En este sistema, una pista CBR tiene frames de tamaño constante; una pista VBR tiene frames de tamaño variable.
- **Binstruct**: archivo EBML propio que contiene el esqueleto reconstructivo del MKV original (PreTracksBlob, TrackEntries, PostTracksBlob, timestamps de cluster, políticas de pista, hashes, idioma original).
- **VFR file** (Variable Frame Record file): archivo independiente por pista, distribuido en el torrent, que contiene la información por frame (tamaño, flags, longitudes NAL si vídeo). Solo presente para pistas con datos variables (todo vídeo y audio VBR).
- **PreTracksBlob**: bytes verbatim del MKV intermedio comprendidos desde el EBML Header hasta inmediatamente antes del Tracks element. Incluye Segment Header e Info, pero no SeekHead (que se regenera).
- **TrackEntries[]**: array en el binstruct donde cada elemento es el blob verbatim de una TrackEntry individual del Tracks element del MKV intermedio.
- **PostTracksBlob**: bytes verbatim del MKV intermedio comprendidos desde inmediatamente después del Tracks element hasta antes del primer Cluster. Típicamente Chapters, Attachments, Tags si existen.
- **Manifest**: archivo de metadatos legibles del torrent en cuatro formatos paralelos (`.ebml`, `.nfo`, `.xml`, `.md`).
- **Sidecar**: archivo de subtítulos distribuido como archivo independiente, no integrado en el MKV virtual sino expuesto en el mismo directorio para auto-detección por reproductores.
- **Parser**: binario independiente, uno por códec, que indexa frame a frame un archivo crudo de pista.
- **Muxer**: componente del lado receptor que reconstruye los bytes del MKV virtual a partir del binstruct y los archivos crudos.
- **Fuser**: componente del lado receptor que implementa el sistema de archivos FUSE bajo el cual viven los MKV virtuales.
- **AVCC / Annex B**: dos formatos alternativos del bitstream H.264/H.265. AVCC usa NAL units con longitud prefijada (formato natural de MKV); Annex B usa start codes (`0x00000001`). mkvextract extrae a Annex B; el muxer debe convertir a AVCC al servir.
- **Variant**: campo de dos dígitos en los nombres de archivo que identifica el tipo de pista cuando varias pistas comparten idioma (primer doblaje profesional, comentario del director, etc.). Su semántica concreta la define el manifest del torrent.
- **File version**: campo de dos dígitos al final del nombre de archivo que indica la versión del archivo en sí. Permite re-emisiones de torrents con archivos corregidos.
- **Idioma original**: idioma de la pista de audio del MKV intermedio marcada con `FlagDefault=1`. Determinado de forma obligatoria por el publicador y almacenado en la sección `Source` del binstruct.
- **Lista jerárquica de idiomas**: lista ordenada por prioridad de hasta 5 códigos ISO 639. El usuario mantiene dos listas independientes: una para audio y otra para subs.
- **Modo `raw` del muxer**: para pistas de audio, los bytes del archivo crudo se copian directamente al payload del Block sin transformación.
- **Modo `transform` del muxer**: para pistas de vídeo, los bytes del archivo crudo (formato Annex B) se convierten a AVCC/HVCC usando la información de NalLengths del VFR antes de copiarse al payload del Block.

---

## 3. Visión general del sistema

### 3.1 Problema que resuelve

La distribución por torrent de archivos MKV con muchas pistas tiene varios problemas en los flujos tradicionales:

- **Sobrecarga**: el receptor descarga todo el archivo aunque solo le interesen una pista de vídeo, una de audio y una de subtítulos.
- **Inflexibilidad**: si el receptor cambia de opinión sobre qué pista quiere, debe descargar otro torrent o re-procesar el archivo.
- **Stream**: arrancar la reproducción antes de tener todo el archivo es complicado y depende de que la cabecera Matroska esté al principio del archivo.
- **Saturación de UI**: con 100+ pistas, los menús de los reproductores se vuelven inmanejables.

videofuser resuelve estos problemas separando la estructura del MKV (que es pequeña y se descarga primero) del payload de las pistas (que es grande y se descarga selectivamente), y filtrando el MKV virtual presentado al reproductor según las preferencias del usuario.

### 3.2 Concepto técnico clave

Un MKV es una estructura EBML jerárquica donde la mayor parte del tamaño en bytes corresponde al payload de los Blocks dentro de los Clusters. La metadata estructural ocupa una fracción mínima del archivo.

Si separamos esa metadata del payload, podemos distribuir cada parte de forma independiente:

- La metadata estructural se serializa en un archivo binstruct compacto (kilobytes a pocos megabytes).
- El payload de cada pista se distribuye como un archivo crudo independiente.

El receptor, conocedor del binstruct y de las preferencias del usuario, puede reconstruir un MKV virtual que contiene solo las pistas relevantes para ese usuario, combinando un subconjunto de las TrackEntries del binstruct con los archivos crudos correspondientes.

### 3.3 Reconstrucción determinista y filtrado

El sistema utiliza **reconstrucción determinista**: el muxer regenera los Clusters y SimpleBlocks del MKV virtual a partir de unas reglas fijas y los timestamps almacenados en el binstruct, sin intentar replicar byte a byte la estructura interna del MKV original.

Como consecuencia:

- El MKV virtual es **funcionalmente equivalente** al original (mismas pistas, mismas duraciones, mismo contenido decodificable, misma sincronización entre pistas) pero **no es bit-exact**. Su tamaño en bytes es ligeramente diferente.
- Los **Cues y SeekHead se regeneran** durante la reconstrucción, basándose en los offsets que el muxer va calculando al emitir clusters.
- **No hay lacing**: cada frame se emite en su propio SimpleBlock. El overhead resultante es del orden del 5-10% sobre el original, irrelevante en la práctica.
- **El conjunto de pistas presentes en el MKV virtual es un subconjunto del MKV intermedio**, determinado en runtime por las preferencias del usuario.

### 3.4 Resiliencia ante datos incompletos

El receptor no necesita haber descargado todos los datos de una pista para reproducirla. El muxer puede servir el MKV virtual a partir de cualquier fracción descargada, rellenando los rangos faltantes según una política configurable:

- **Modo bloqueante (default)**: el muxer bloquea la lectura del FUSE hasta que llegan los datos. El reproductor se pausa brevemente; al llegar continúa limpiamente.
- **Modo con timeout**: como el bloqueante pero con un límite temporal; pasado ese límite se rellena con ceros alineados a frame.
- **Modo no-bloqueante**: lecturas devuelven ceros inmediatamente si no hay datos.

En todos los modos, los rangos rellenados con ceros se invalidan en el caché del kernel mediante `fuse::Notifier::inval_inode` cuando llegan los datos reales.

---

## 4. Arquitectura

### 4.1 Componentes del lado publicador

- **ffmpeg** (herramienta externa): reescala la pista de vídeo del MKV original a resoluciones inferiores.
- **mkvmerge** (herramienta externa de MKVToolNix): añade las pistas de vídeo reescaladas al MKV original. Genera el MKV intermedio.
- **mkvextract** (herramienta externa de MKVToolNix): extrae pistas crudas del MKV original (subtítulos) y del MKV intermedio (vídeo y audio).
- **parser-h264, parser-h265, parser-av1, parser-mp3, parser-aac, parser-dts, parser-truehd, parser-ac3, parser-eac3** (binarios del proyecto): indexan los archivos crudos a nivel de frame, produciendo los archivos VFR.
- **binstruct** (binario del proyecto): genera el archivo binstruct.ebml a partir del MKV intermedio y los archivos VFR. Realiza validaciones obligatorias sobre el MKV intermedio.

### 4.2 Componentes del lado receptor

El receptor consiste en un único proceso demonio, `videofuser`, que internamente orquesta varios componentes:

- **Watcher**: monitoriza la lista de torrents del cliente del usuario y detecta los reconocidos.
- **Registry**: mantiene el estado de cada torrent reconocido.
- **Muxer (lib `videofuser-muxer`)**: implementa la reconstrucción del MKV virtual a partir del binstruct, los archivos crudos y un filtro de pistas.
- **Fuser (lib `videofuser-fs`)**: implementa el sistema de archivos FUSE multiplexor.
- **IPC server**: socket Unix que recibe comandos de instancias secundarias del binario.
- **Track controller** (fase posterior): observa lecturas del FUSE para inferir qué piezas priorizar.
- **Preferences store**: prefs del usuario persistidas en disco. Incluye dos listas jerárquicas independientes (audio y subs).
- **TorrentClientAdapter (trait)**: abstracción formal sobre el cliente torrent del usuario. El sistema no se acopla directamente a un cliente específico (qBittorrent, Transmission, Deluge, rTorrent); en lugar de ello, define un trait con las operaciones necesarias (listar torrents, consultar progreso por archivo, establecer prioridad por pieza, recibir notificaciones de piezas completadas). Cada cliente soportado tiene su propia implementación del trait. El Watcher y el Track controller hablan con el cliente exclusivamente a través de este trait, lo que permite añadir soporte para nuevos clientes sin tocar el resto del sistema. La interfaz del trait y su contrato están en el capítulo 17.

### 4.3 Flujo de datos

1. El publicador parte de un MKV original con sus pistas de vídeo, audio y subtítulos.
2. ffmpeg reescala el vídeo a 7 resoluciones adicionales.
3. mkvextract saca las pistas de subtítulos del MKV original a archivos sidecar.
4. mkvmerge crea un MKV intermedio combinando el MKV original (sin subtítulos) con los archivos de vídeo reescalados. El publicador asegura que exactamente una pista de audio tiene `FlagDefault=1`.
5. mkvextract saca las pistas de vídeo y audio del MKV intermedio a archivos crudos.
6. Los parsers procesan cada archivo crudo de vídeo y audio generando archivos VFR.
7. `binstruct gen` lee el MKV intermedio (con validaciones), los VFR y produce binstruct.ebml.
8. El publicador empaqueta toda la estructura en un directorio.
9. El publicador genera el .torrent y lo distribuye.
10. El receptor recibe el .torrent y lo añade a su cliente.
11. videofuser detecta el torrent reconocido, descarga el binstruct y aplica las prefs del usuario para determinar qué pistas exponer y descargar.
12. El reproductor del usuario abre el MKV virtual; sus lecturas se traducen en bytes generados por el muxer.
13. El usuario puede cambiar de pista en el reproductor o cambiar prefs en cualquier momento.

---

## 5. Lado publicador: pipeline

### 5.1 Reescalado del vídeo

A partir de la pista de vídeo del MKV original, el publicador genera 7 reescalados a las siguientes resoluciones: 1440p, 1080p, 720p, 540p, 480p, 360p, 240p. Junto con el vídeo original a 4K, el sistema dispone de 8 pistas de vídeo.

La herramienta recomendada es ffmpeg. Cada reescalado se produce a un archivo crudo en uno de los códecs soportados: H.264 (`.h264`), H.265 (`.h265` o `.hevc`), AV1 (`.av1`).

### 5.2 Extracción de subtítulos del MKV original

Con `mkvextract tracks`, el publicador extrae las pistas de subtítulos del MKV original a archivos sidecar. Solo se admiten cuatro formatos: SRT, ASS, SSA, WebVTT. Si el MKV original contiene subtítulos en otros formatos, el publicador debe convertirlos previamente o descartarlos.

### 5.3 Re-mux: generación del MKV intermedio

Con `mkvmerge`, el publicador combina el MKV original con los 7 archivos de vídeo reescalados, generando un MKV intermedio. **Crítico**: el MKV intermedio debe ser generado **sin las pistas de subtítulos** (opción `--no-subtitle-tracks`).

**Requisito obligatorio**: el MKV intermedio resultante debe tener exactamente una pista de audio con `FlagDefault=1`. Esta pista define el idioma original del contenido. El publicador debe asegurarse de:

- Que ninguna otra pista de audio del MKV intermedio tenga `FlagDefault=1`.
- Que la pista marcada corresponde realmente al idioma original del contenido (no a la versión doblada o cualquier otra).
- Que el campo `Language` de esa pista esté correctamente poblado con el código ISO 639 del idioma original.

mkvmerge permite controlar `FlagDefault` con la opción `--default-track <track-id>:yes/no`. El publicador debe usar esa opción para garantizar el cumplimiento del requisito.

El MKV intermedio resultante contiene:

- 8 pistas de vídeo.
- N pistas de audio (las del MKV original).
- 0 pistas de subtítulos.
- Exactamente una pista de audio con `FlagDefault=1`, que es la del idioma original.

### 5.4 Extracción de pistas crudas del MKV intermedio

Con `mkvextract tracks`, el publicador extrae las pistas de vídeo y audio del MKV intermedio a archivos crudos.

Para vídeo, mkvextract genera bitstreams en formato Annex B. Para audio, los formatos generados son los esperados por cada códec.

### 5.5 Indexado por parsers

Por cada archivo crudo de vídeo o audio, el publicador invoca el parser correspondiente al códec. El parser:

- Recorre el archivo crudo.
- Identifica los límites de cada frame.
- Por cada frame, registra: `frame_size`, flags (keyframe, etc.), duración, y para vídeo lista de longitudes NAL/OBU.
- Detecta si la pista es CBR o VBR.
- Si CBR audio: no genera archivo VFR; emite metadatos en stderr en formato JSON.
- Si VBR (siempre todo vídeo, audio variable): genera el archivo VFR.

### 5.6 Generación del binstruct

`binstruct gen` lee el MKV intermedio cluster a cluster, realiza validaciones obligatorias, recoge los archivos VFR y produce el archivo `binstruct.ebml`.

Validaciones obligatorias antes de generar:

- Existe exactamente una pista de audio con `FlagDefault=1`. Si hay cero o más de una, se aborta con error explicativo.
- El campo `Language` de la pista marcada como default está poblado con un código ISO 639 válido. Si no, se aborta.
- Cada pista de audio o vídeo del MKV intermedio tiene un archivo crudo correspondiente identificable por nomenclatura.
- Cada pista identificada como VBR por su parser tiene un archivo VFR correspondiente.
- **Validación de consistencia CBR**: para cada pista marcada CBR, el tamaño en bytes del archivo crudo debe ser exactamente igual a `FrameCount × CbrFrameSize`. Si no coincide, se aborta con error indicando que el parser ha clasificado erróneamente la pista como CBR (típicamente por encontrar un último frame incompleto que el parser no detectó), o que el archivo crudo está corrupto. Esta validación atrapa una clase de errores catastróficos en runtime: si un audio CBR tiene en realidad un frame de tamaño distinto, los offsets calculados aritméticamente se desalinean y el muxer sirve datos basura.
- **Captura de versiones de herramientas**: `binstruct gen` registra las versiones exactas de ffmpeg, mkvmerge y mkvextract usadas en el pipeline (consultando `--version` de cada una y parseando la salida). Estas versiones se incluyen en la sección `Source` del binstruct mediante el campo `BuildTools`. Permite reproducibilidad y diagnóstico de regresiones futuras.

Operaciones internas:

1. Parsea el MKV intermedio con `ebml-iterable`.
2. Captura los bytes verbatim de las secciones EBML pre-Tracks, las TrackEntries individuales, y las secciones post-Tracks (Chapters, Attachments, Tags).
3. Extrae los timestamps de inicio de cada Cluster.
4. Por cada pista, sintetiza un `TrackPolicy` con su metadata.
5. Determina el `original_language` desde la TrackEntry con `FlagDefault=1`.
6. Captura las versiones de las herramientas externas usadas en el pipeline.
7. Empaqueta todo en un archivo EBML propio según el schema del capítulo 7.
8. Comprime el archivo resultante con zstd.

### 5.7 Generación del manifest

El publicador genera los cuatro archivos manifest en sus respectivos formatos (`.ebml`, `.nfo`, `.xml`, `.md`).

Tras generarlos, se recomienda ejecutar `binstruct verify-manifest <directorio_info>` para verificar la coherencia entre los cuatro formatos. Este subcomando del binario `binstruct`:

1. Parsea el `manifest.ebml` (formato canónico) extrayendo todos los campos.
2. Parsea los otros tres formatos (`.nfo`, `.xml`, `.md`) intentando extraer los mismos campos.
3. Compara campo a campo. Reporta discrepancias en stderr y termina con código de salida no-cero si encuentra divergencias.

Campos verificados: título, año, hash del MKV original, idioma original, lista completa de pistas (track_id, tipo, idioma, códec, variant), y leyenda de variants. Diferencias en formato (espaciado, capitalización en descripciones libres) se ignoran; solo se reportan diferencias semánticas.

`verify-manifest` no es obligatorio en el pipeline (los manifests no canónicos son cortesía del publicador), pero se recomienda como paso de QA antes de empaquetar el torrent.

### 5.8 Empaquetado

El publicador organiza todos los archivos en la estructura de directorios canónica.

### 5.9 Generación del torrent

Con cualquier herramienta estándar, el publicador genera un archivo .torrent del directorio empaquetado.

---

## 6. Estructura del torrent

### 6.1 Layout de directorios

La estructura canónica del directorio raíz del torrent, donde `<base>` representa el nombre del MKV original sin extensión:

```
<torrent_root>/
├── info/
│   ├── <base>_binstruct_<vv>.ebml
│   ├── <base>_manifest_<vv>.ebml
│   ├── <base>_manifest_<vv>.nfo
│   ├── <base>_manifest_<vv>.xml
│   ├── <base>_manifest_<vv>.md
│   └── vfr/
│       ├── <base>_v<NN>_<vv>.vfr[.zst]
│       ├── ...
│       ├── <base>_a<NNN>_<variant>_<vv>.vfr[.zst]
│       └── ...
├── video/
│   └── <base>_v<NN>_<res>_<vv>.<ext>
├── audio/
│   └── <base>_a<NNN>_<lang>_<variant>_<vv>.<ext>
└── subs/
    └── <base>_s<NNN>_<lang>_<variant>_<vv>.<ext>
```

### 6.2 Convención de nombrado

Todos los archivos del torrent llevan como prefijo el nombre del MKV original sin extensión (`<base>`).

#### 6.2.1 Reglas comunes

- **`<base>`**: nombre del MKV original sin extensión, preservado tal cual.
- **`<vv>`**: dos dígitos decimales de versión del archivo.

#### 6.2.2 Campos específicos

- **`<NN>`**: track_id codificado a dos dígitos (`00`-`99`).
- **`<NNN>`**: track_id codificado a tres dígitos (`000`-`999`).
- **`<res>`**: resolución del vídeo (`4k`, `1440p`, `1080p`, `720p`, `540p`, `480p`, `360p`, `240p`).
- **`<lang>`**: código de idioma ISO 639-2/3, en minúsculas.
- **`<variant>`**: dos dígitos decimales que identifican el tipo de pista cuando varias pistas comparten idioma.
- **`<ext>`**: extensión correspondiente al códec o formato.

#### 6.2.3 Patrones por tipo de archivo

| Tipo de archivo | Patrón | Ejemplo |
|---|---|---|
| binstruct | `<base>_binstruct_<vv>.ebml` | `Pelicula_binstruct_00.ebml` |
| manifest (4 formatos) | `<base>_manifest_<vv>.{ebml,nfo,xml,md}` | `Pelicula_manifest_00.ebml` |
| VFR de vídeo (sin comprimir) | `<base>_v<NN>_<vv>.vfr` | `Pelicula_v00_00.vfr` |
| VFR de vídeo (comprimido) | `<base>_v<NN>_<vv>.vfr.zst` | `Pelicula_v00_00.vfr.zst` |
| VFR de audio (sin comprimir) | `<base>_a<NNN>_<variant>_<vv>.vfr` | `Pelicula_a000_00_00.vfr` |
| VFR de audio (comprimido) | `<base>_a<NNN>_<variant>_<vv>.vfr.zst` | `Pelicula_a000_00_00.vfr.zst` |
| Vídeo crudo | `<base>_v<NN>_<res>_<vv>.<ext>` | `Pelicula_v00_4k_00.h265` |
| Audio crudo | `<base>_a<NNN>_<lang>_<variant>_<vv>.<ext>` | `Pelicula_a000_en_00_00.eac3` |
| Subtítulo | `<base>_s<NNN>_<lang>_<variant>_<vv>.<ext>` | `Pelicula_s000_en_00_00.srt` |

#### 6.2.4 Notas sobre VFR

- VFR de vídeo: no llevan campo `<variant>`.
- VFR de audio: llevan `<variant>` pero no `<lang>`.
- VFR de pistas CBR: ausentes.
- VFR comprimidos: llevan extensión adicional `.zst`. El demonio detecta la presencia de la extensión y descomprime con zstd antes de procesar.

### 6.3 Reglas de presencia

#### 6.3.1 Archivos siempre presentes

- `info/<base>_binstruct_<vv>.ebml`
- Los cuatro formatos de manifest.
- Los 8 archivos `info/vfr/<base>_v<NN>_<vv>.vfr[.zst]`.
- Los archivos de `video/`, `audio/` y `subs/`.

#### 6.3.2 Archivos condicionales

- `info/vfr/<base>_a<NNN>_<variant>_<vv>.vfr[.zst]`: presente si y solo si la pista de audio correspondiente es VBR.

#### 6.3.3 Validación de la estructura

Un torrent es reconocido como válido si:

1. Existe el directorio `info/`.
2. Hay exactamente un archivo cuyo nombre coincide con el patrón `*_binstruct_*.ebml`.
3. Hay exactamente un conjunto de cuatro archivos manifest con el mismo `<base>` y `<vv>`.
4. Los nombres de archivo en `info/vfr/`, `video/`, `audio/`, `subs/` siguen los patrones esperados.

Si alguna condición falla, el torrent se ignora.

---

## 7. Formato del binstruct

### 7.1 Generalidades

El binstruct es un archivo en formato EBML propio. Reutiliza la sintaxis de EBML pero define IDs y semántica propios sin colisión con Matroska. El archivo se comprime con zstd.

**Versión del schema**: 1.

### 7.2 Codificación EBML

EBML usa enteros de longitud variable (VINT) para IDs y sizes. Los IDs del binstruct se eligen de modo que ocupen 1 o 2 bytes.

### 7.3 Tabla de IDs

| Elemento | ID (hex) | Tamaño VINT | Tipo |
|---|---|---|---|
| BinstructFile | `0x80` | 1 byte | Master |
| Header | `0x81` | 1 byte | Master |
| Magic | `0x82` | 1 byte | Binary (4 bytes) |
| Version | `0x83` | 1 byte | UInt (varint) |
| ConfigFlags | `0x84` | 1 byte | Binary (1 byte) |
| Source | `0x85` | 1 byte | Master |
| OriginalMkvHash | `0x86` | 1 byte | Binary (32 bytes) |
| PublisherInfo | `0x87` | 1 byte | UTF-8 string |
| CreationTimestamp | `0x88` | 1 byte | UInt (u64 ms unix) |
| OriginalLanguage | `0x89` | 1 byte | UTF-8 string (ISO 639) |
| OriginalDefaultTrackId | `0x8A` | 1 byte | UInt (varint) |
| BuildTools | `0xA0` | 1 byte | Master |
| ToolEntry | `0xA1` | 1 byte | Master |
| ToolName | `0xA2` | 1 byte | UTF-8 string |
| ToolVersion | `0xA3` | 1 byte | UTF-8 string |
| MkvSkeleton | `0x8B` | 1 byte | Master |
| PreTracksBlob | `0x8C` | 1 byte | Binary (raw bytes) |
| TrackEntries | `0x8D` | 1 byte | Master |
| TrackEntryRecord | `0x8E` | 1 byte | Master |
| TrackEntryId | `0x8F` | 1 byte | UInt (varint) |
| TrackEntryBytes | `0x90` | 1 byte | Binary (raw bytes) |
| PostTracksBlob | `0x91` | 1 byte | Binary (raw bytes; puede estar vacío) |
| ClusterTimestamps | `0x92` | 1 byte | Master |
| ClusterCount | `0x93` | 1 byte | UInt (varint) |
| ClusterTimestampDeltas | `0x94` | 1 byte | Binary (array of varints) |
| TrackPolicies | `0x95` | 1 byte | Master |
| TrackPolicy | `0x96` | 1 byte | Master |
| TrackId | `0x97` | 1 byte | UInt (varint) |
| CodecType | `0x98` | 1 byte | UInt (1 byte: 0=video, 1=audio) |
| LanguageCode | `0x99` | 1 byte | UTF-8 string (ISO 639) |
| FrameCount | `0x9A` | 1 byte | UInt (varint) |
| IsVbr | `0x9B` | 1 byte | UInt (1 byte boolean) |
| FrameDuration | `0x9C` | 1 byte | UInt (varint, track timebase units) |
| CbrFrameSize | `0x9D` | 1 byte | UInt (varint) |
| RawFileHash | `0x9E` | 1 byte | Binary (32 bytes) |
| VfrFileHash | `0x9F` | 1 byte | Binary (32 bytes) |

IDs adicionales (2 bytes, rango `0x4000`-`0x7FFE`) reservados para extensiones futuras.

### 7.4 Estructura del archivo

```
BinstructFile [0x80]
├── Header [0x81]
│   ├── Magic [0x82] = 0x56 0x46 0x55 0x53 ("VFUS")
│   ├── Version [0x83] = 1
│   └── ConfigFlags [0x84]
├── Source [0x85]
│   ├── OriginalMkvHash [0x86]
│   ├── PublisherInfo [0x87]
│   ├── CreationTimestamp [0x88]
│   ├── OriginalLanguage [0x89]              (ISO 639 code)
│   ├── OriginalDefaultTrackId [0x8A]        (track_id de la pista marcada FlagDefault=1)
│   └── BuildTools [0xA0]
│       └── ToolEntry [0xA1] × N
│           ├── ToolName [0xA2]              ("ffmpeg", "mkvmerge", "mkvextract")
│           └── ToolVersion [0xA3]           (string libre devuelto por --version)
├── MkvSkeleton [0x8B]
│   ├── PreTracksBlob [0x8C]                 (EBML Header + Segment Header + Info, verbatim)
│   ├── TrackEntries [0x8D]
│   │   └── TrackEntryRecord [0x8E] × N
│   │       ├── TrackEntryId [0x8F]          (track_id)
│   │       └── TrackEntryBytes [0x90]       (bytes verbatim de la TrackEntry del MKV intermedio)
│   └── PostTracksBlob [0x91]                (Chapters + Attachments + Tags, verbatim; puede estar vacío)
├── ClusterTimestamps [0x92]
│   ├── ClusterCount [0x93]
│   └── ClusterTimestampDeltas [0x94]
└── TrackPolicies [0x95]
    └── TrackPolicy [0x96] × N
        ├── TrackId [0x97]
        ├── CodecType [0x98]
        ├── LanguageCode [0x99]
        ├── FrameCount [0x9A]
        ├── IsVbr [0x9B]
        ├── FrameDuration [0x9C]
        ├── CbrFrameSize [0x9D]               (presente si !IsVbr)
        ├── RawFileHash [0x9E]
        └── VfrFileHash [0x9F]                (presente si IsVbr o CodecType==video)
```

### 7.5 Detalle de cada elemento

#### Header

- **Magic** (4 bytes): valor fijo `0x56 0x46 0x55 0x53` ("VFUS").
- **Version** (varint): versión del schema. **Valor 1 para esta especificación**.
- **ConfigFlags** (1 byte): bit 0 indica si el archivo está comprimido con zstd.

#### Source

- **OriginalMkvHash**: SHA-256 del MKV original.
- **PublisherInfo**: información libre sobre el publicador.
- **CreationTimestamp**: timestamp Unix en milisegundos.
- **OriginalLanguage**: código ISO 639 del idioma original. Extraído del campo `Language` de la TrackEntry del MKV intermedio que tenía `FlagDefault=1`. Obligatorio.
- **OriginalDefaultTrackId**: track_id de la pista que en el MKV intermedio tenía `FlagDefault=1`. Permite al receptor identificar la pista original sin tener que parsear las TrackEntry blobs. Obligatorio.
- **BuildTools**: master element que contiene una lista de `ToolEntry`, cada uno con `ToolName` y `ToolVersion`. Captura las versiones exactas de las herramientas externas usadas en el pipeline del publicador (ffmpeg, mkvmerge, mkvextract). Las versiones se obtienen ejecutando `<tool> --version` y registrando la primera línea de la salida. Permite reproducibilidad del binstruct y diagnóstico de regresiones cuando los publicadores usan versiones distintas de las herramientas. Obligatorio: el binstruct debe contener al menos las entradas para `ffmpeg`, `mkvmerge` y `mkvextract`.

#### MkvSkeleton

Sección compuesta que permite al muxer regenerar el Tracks element del MKV virtual con un subconjunto de las TrackEntry originales.

- **PreTracksBlob**: bytes verbatim del MKV intermedio desde el inicio del archivo hasta inmediatamente antes del Tracks element. Incluye:
  - EBML Header completo.
  - Segment Header (con su Size, que será sobrescrito por el muxer si emite Size declarado, o emitido como "unknown size" según política).
  - Info element (con TimecodeScale, Duration, etc.) verbatim.
  - **No incluye SeekHead**. SeekHead se regenera siempre por el muxer al final del archivo.

- **TrackEntries**: master element que contiene un `TrackEntryRecord` por cada pista de vídeo y audio del MKV intermedio.
  - **TrackEntryId**: track_id, redundante con el TrackPolicy correspondiente pero útil para lookup directo.
  - **TrackEntryBytes**: bytes verbatim del elemento TrackEntry tal como aparecía en el Tracks element del MKV intermedio. Esto incluye TrackNumber, TrackUID, TrackType, FlagEnabled, FlagDefault, FlagForced, Language, CodecID, CodecPrivate, etc., todos verbatim.

- **PostTracksBlob**: bytes verbatim del MKV intermedio desde inmediatamente después del Tracks element hasta inmediatamente antes del primer Cluster. Típicamente Chapters, Attachments, Tags. Puede estar vacío si el MKV intermedio no tenía esas secciones.

#### ClusterTimestamps

Como en v1: array de timestamps de inicio de cluster codificado como deltas.

#### TrackPolicies

Como en v1, con un campo añadido:

- **LanguageCode**: código ISO 639 del idioma de la pista. Para vídeo es típicamente "und" o vacío. Para audio es el campo Language de la TrackEntry. Permite al receptor filtrar pistas por idioma sin parsear las TrackEntry blobs.

### 7.6 Compresión

El archivo binstruct.ebml se comprime con zstd nivel 9. La descompresión la realiza el receptor al cargar.

### 7.7 Tamaño esperado

Para 8 vídeos + 103 audios + 2 horas: ~50-100 KB sin comprimir, ~25-50 KB comprimido. La parte mayor son las TrackEntry blobs (CodecPrivate puede ser de cientos de bytes por pista en algunos códecs).

---

## 8. Formato de los archivos VFR

### 8.1 Generalidades

Cada archivo VFR contiene los datos por frame de una pista que requiere indexado: todas las pistas de vídeo, y aquellas de audio marcadas como VBR. Los CBR no tienen archivo VFR.

Pueden estar comprimidos con zstd. Cuando lo están, llevan extensión `.vfr.zst`. El demonio detecta automáticamente la presencia o ausencia de compresión por la extensión.

### 8.2 Estructura

```
VfrFile
├── Magic (4 bytes ASCII)          = "VFRF"
├── Version (1 byte)               = 1
├── Flags (1 byte)                 (bit 0: tiene tabla NalLengths; resto reservados)
├── Reserved (2 bytes)             = 0
├── FrameCount (8 bytes u64 LE)
├── NalLengthsOffset (8 bytes u64 LE)  (offset desde el inicio del archivo a la tabla NalLengths; 0 si no hay)
├── Frames[FrameCount]             (FrameCount × 8 bytes)
└── NalLengths[]                   (variable, presente solo para vídeo)
```

### 8.3 Records de frame (8 bytes cada uno)

| Campo | Tamaño | Tipo | Descripción |
|---|---|---|---|
| frame_size | 4 bytes | u32 LE | Tamaño del frame en el archivo crudo, en bytes |
| flags | 1 byte | u8 | Bit 0: keyframe; bit 1: has_nal_lengths; resto reservados |
| nal_count | 1 byte | u8 | Número de NAL units (vídeo) o 0 (audio) |
| duration_delta | 2 bytes | i16 LE | Delta de duración respecto a `FrameDuration` de TrackPolicy |

El offset del frame K en el archivo crudo se calcula como suma acumulada de `frame_size` de los frames `0..K-1`. Para acceso aleatorio rápido, el muxer construye al cargar un índice acumulado cada 1024 frames.

### 8.4 Tabla NalLengths (solo vídeo)

Array empaquetado de varints. Para encontrar las longitudes NAL del frame K:

1. Sumar los `nal_count` de los frames `0..K-1` → offset_acumulado_nal.
2. Saltar `offset_acumulado_nal` varints en la tabla.
3. Leer `nal_count` varints empezando ahí.

El muxer construye un índice acumulado análogo al de offsets de frame.

### 8.5 Tamaño esperado

- Vídeo: ~13 bytes por frame (8 base + ~5 NAL avg). 172800 frames × 13 ≈ 2.2 MB por pista.
- Audio VBR: 8 bytes por frame. 340000 × 8 = 2.7 MB por pista.

### 8.6 Compresión

Cuando se aplica, los archivos VFR se comprimen con zstd. La extensión cambia a `.vfr.zst`. El demonio descomprime al cargar.

---

## 9. Formatos del manifest

### 9.1 manifest.ebml (canónico)

El manifest que el demonio parsea. Formato EBML propio. Contenido:

- **Title**: título del contenido.
- **Year**: año de producción.
- **OriginalMkvFilename**: nombre del MKV original.
- **OriginalMkvHash**: SHA-256 del MKV original (debe coincidir con el del binstruct).
- **OriginalLanguage**: código ISO 639 (debe coincidir con el `OriginalLanguage` del binstruct).
- **SystemVersion**: versión del sistema videofuser.
- **Publisher**: nombre del publicador.
- **TrackList**: lista de pistas con metadata legible.
- **VariantsLegend**: tabla de descripciones para los variants usados.

### 9.2 manifest.nfo

Formato compatible con Kodi, Plex, Jellyfin para ingesta automática en bibliotecas multimedia. Estructura XML siguiendo el esquema NFO estándar:

```xml
<movie>
  <title>Pelicula Maravillosa 2</title>
  <year>1995</year>
  <plot>...</plot>
  <director>John John</director>
  <director>Mark Robinson</director>
  <runtime>120</runtime>
  ...
</movie>
```

El demonio `videofuser` no usa este archivo; existe puramente para integración con software externo de gestión multimedia. El publicador es responsable de su contenido y de su correspondencia con los datos del manifest.ebml.

### 9.3 manifest.xml

XML genérico con un esquema definido por el sistema videofuser, pensado para herramientas que consuman XML estructurado. Estructura:

```xml
<videofuser-manifest version="1">
  <title>Pelicula Maravillosa 2</title>
  <year>1995</year>
  <hash>...</hash>
  <publisher>...</publisher>
  <original-language>en</original-language>
  <tracks>
    <track id="0" type="video" resolution="4k" codec="h265" />
    <track id="1" type="video" resolution="1440p" codec="h265" />
    ...
    <track id="100" type="audio" language="es" codec="eac3" variant="00" />
    <track id="101" type="audio" language="es" codec="eac3" variant="01" description="Comentarios" />
    ...
    <track id="200" type="subtitle" language="es" format="srt" variant="00" />
    ...
  </tracks>
  <variants>
    <variant id="00" description="Doblaje principal" />
    <variant id="01" description="Comentarios del director" />
    ...
  </variants>
</videofuser-manifest>
```

El demonio no usa este archivo. Está disponible para herramientas de terceros que prefieran XML.

### 9.4 manifest.md

Formato Markdown legible por humanos. Estructura libre, pero la convención recomendada es:

```markdown
# Pelicula Maravillosa 2 (1995)

## Información general
- **Año**: 1995
- **Hash del MKV original**: ...
- **Idioma original**: en
- **Publicador**: ...

## Pistas de vídeo
| Track ID | Resolución | Códec |
|---|---|---|
| 00 | 4K | H.265 |
| 01 | 1440p | H.265 |
| ... | ... | ... |

## Pistas de audio
| Track ID | Idioma | Códec | Variant | Descripción |
|---|---|---|---|---|
| 000 | en | E-AC-3 Atmos | 00 | Doblaje original |
| 001 | en | TrueHD | 00 | Doblaje original |
| 002 | es | E-AC-3 | 00 | Doblaje castellano |
| ... | ... | ... | ... | ... |

## Pistas de subtítulos
| Track ID | Idioma | Formato | Variant |
|---|---|---|---|
| 000 | en | SRT | 00 |
| ... | ... | ... | ... |
```

Permite al publicador documentar el contenido en lenguaje natural y a usuarios curiosos navegar el torrent y entender qué está descargando. El demonio no usa este archivo.

### 9.5 Coherencia entre formatos

Los cuatro formatos deben contener información coherente. La fuente de verdad para el sistema es `manifest.ebml`.

---

## 10. Los parsers

### 10.1 Generalidades

Binarios independientes, uno por códec. Solo se ejecutan en el lado publicador. Indexan los archivos crudos de pistas a nivel de frame.

Parsers cubiertos en MVP: `parser-h264`, `parser-aac`, `parser-ac3`. Planificados: `parser-h265`, `parser-av1`, `parser-mp3`, `parser-dts`, `parser-truehd`, `parser-eac3`.

### 10.2 Contrato CLI uniforme

```
parser-<codec> index --variant <v> --version <ver> [opciones específicas] <input>
```

Salida: stdout binario con el archivo VFR completo. Stderr para logs y, en caso de pista CBR, metadatos JSON.

### 10.3 Detección de CBR vs VBR

Si todos los frames tienen el mismo `frame_size`, la pista es CBR. El parser no escribe VFR en stdout y emite a stderr:

```json
{"is_vbr": false, "frame_count": 340000, "cbr_frame_size": 768, "frame_duration_ns": 32000000, "language": "es"}
```

El publicador captura esta salida para integrarla en el TrackPolicy del binstruct.

### 10.4 Parsers específicos

#### parser-h264

- Variantes: `avc` (codifica el bitstream en NAL units estándar; única en uso).
- Versiones: `1` (H.264 estándar).
- Opciones: `--profile {baseline, main, high}`, `--level <decimal>`.

Espera el archivo crudo en formato Annex B con start codes de 4 bytes. Identifica frames buscando NAL units de tipo IDR (5) y no-IDR slice (1). Considera un frame completo cuando termina la secuencia de NAL units que componen el frame.

Por cada frame:

- Cuenta NAL units que lo forman.
- Calcula `frame_size` total (bytes Annex B desde el start code del primer NAL hasta el byte previo al start code del siguiente frame).
- Lista las longitudes de cada NAL unit dentro del frame (longitud del payload del NAL, sin start code).
- Detecta si es keyframe por la presencia de NAL unit IDR.
- Calcula `duration_delta` basándose en información SEI (pic_timing) o en regularidad detectada del frame rate.

#### parser-h265 (planificado)

- Variantes: `hevc`.
- Versiones: `1` (HEVC estándar), `2` (HEVC con extensiones tier).
- Opciones: `--profile {main, main10, main_still}`, `--level <decimal>`.

Similar a parser-h264 pero para H.265 / HEVC. Identifica NAL units por tipo (IRAP, IDR_W_RADL, IDR_N_LP, etc.). Detecta keyframes en NAL units de tipo IRAP.

#### parser-av1 (planificado)

- Variantes: `obu` (OBU stream con length fields).
- Versiones: `1`.
- Opciones: `--profile {main, high, professional}`.

Trabaja con OBUs (Open Bitstream Units) en lugar de NAL units. Cada frame puede contener varios OBU (sequence header, frame header, tile group, etc.). El parser identifica los límites de frame por OBU de tipo OBU_FRAME o OBU_FRAME_HEADER seguido de OBU_TILE_GROUP. La conversión OBU stream ↔ OBU-en-MKV difiere ligeramente de Annex B/AVCC: en MKV los OBUs no llevan start codes, sino que se concatenan directamente. Los `nal_lengths` del VFR para AV1 corresponden a longitudes de OBU.

#### parser-aac

- Variantes: `lc` (Low Complexity), `he` (High Efficiency / HE-AAC v1), `hev2` (HE-AAC v2 con Parametric Stereo).
- Versiones: `mpeg2`, `mpeg4`.
- Opciones: ninguna específica.

Espera el archivo crudo en formato ADTS. Identifica frames por sus syncwords ADTS (los 12 bits altos a `0xFFF`). Cada frame es independiente (todos son keyframes en términos MKV).

Por cada frame:

- Lee la cabecera ADTS de 7 o 9 bytes según la presencia de CRC.
- Extrae `frame_length` del campo correspondiente.
- Marca como keyframe.
- `nal_count = 0` (audio).
- `duration_delta = 0` (AAC tiene duración constante por frame).

Si todos los `frame_length` son iguales, marca CBR.

#### parser-ac3

- Variantes: `ac3` (la única).
- Versiones: `1` (estándar ATSC A/52).
- Opciones: ninguna específica.

Espera el archivo crudo en formato AC-3 con syncwords `0x0B77` al inicio de cada frame. El tamaño de cada frame se determina por la combinación de `bsid`, `fscod` y `frmsizecod` en la cabecera, según las tablas del estándar.

Cada frame es independiente. AC-3 es típicamente CBR. El parser compara `frame_size` de todos los frames; si son idénticos, marca CBR.

#### parser-eac3 (planificado)

- Variantes: `eac3` (E-AC-3 estándar).
- Versiones: `1` (estándar ATSC A/52 anexo E).
- Opciones: `--substream-mode {independent, dependent, mixed}`.

E-AC-3 es más complejo que AC-3 por la posibilidad de múltiples substreams (independent + dependent) que conjuntamente forman un frame "extendido". El parser puede agrupar los substreams como un único frame lógico (modo `mixed`) o tratarlos por separado (modo `independent`/`dependent`).

#### parser-dts (planificado)

- Variantes: `core`, `hd_hra` (High Resolution Audio), `hd_ma` (Master Audio).
- Versiones: `1`, `2`.
- Opciones: `--substream-mode {core_only, full}`.

DTS Core es relativamente sencillo (frame con syncword `0x7FFE8001`). DTS-HD añade un substream HD que contiene los datos lossless o de mayor resolución. El parser para DTS-HD identifica el frame DTS Core seguido del substream HD asociado.

#### parser-truehd (planificado)

- Variantes: `truehd` (Dolby TrueHD).
- Versiones: `1`.
- Opciones: `--atmos-substream {include, ignore}`.

Dolby TrueHD es lossless con frame structure compleja. Cada frame comienza con un major sync header (en frames de presentation rate) o con un minor sync (entre majors). Posibles substreams Atmos extienden el formato.

#### parser-mp3

- Variantes: `mpeg1_layer3`, `mpeg2_layer3`.
- Versiones: `1`, `2`.
- Opciones: ninguna específica.

Identifica frames por syncwords MPEG audio (los 11 bits altos a `0x7FF`). Cada frame es independiente. MP3 puede ser CBR o VBR; el parser detecta automáticamente comparando los `frame_size`.

### 10.5 Conversión de formato (vídeo)

Los archivos crudos extraídos por mkvextract para vídeo están en formato Annex B. En el MKV virtual, los Block payloads de vídeo deben estar en formato AVCC/HVCC.

La conversión se realiza en el muxer (capítulo 13.5).

### 10.6 CodecPrivate y headers inyectados

Los parsers detectan headers SPS/PPS inyectados al inicio de archivos crudos por mkvextract y los excluyen del primer frame indexado.

---

## 11. El demonio videofuser

### 11.1 Patrón single-instance

Una instancia única por usuario. Múltiples invocaciones del binario se conectan al demonio existente vía socket Unix en `/run/user/<uid>/videofuser.sock`. El subcomando explícito `videofuser daemon` fuerza el modo demonio.

### 11.2 CLI

```
videofuser                              # arranca demonio si no existe
videofuser daemon                       # arranque explícito
videofuser status                       # estado de torrents y prefs
videofuser prefs set <key=value>...     # establece preferencias
videofuser prefs get [<key>]            # consulta preferencias
videofuser mount <torrent_id>           # fuerza watcher
videofuser unmount <torrent_id>
videofuser shutdown
videofuser version
```

Las claves de preferencias incluyen:

- **`audio_langs`**: lista jerárquica de hasta 5 idiomas preferidos para audio. Ejemplo: `audio_langs=es,en,it,fr,de`. Orden importa: la primera es la más preferida.
- **`sub_langs`**: lista jerárquica de hasta 5 idiomas preferidos para subs, **independiente** de la anterior. Ejemplo: `sub_langs=es,en,fr,it,pt`.
- **`audio_codec`**: lista de códecs de audio preferidos por prioridad. Ejemplo: `audio_codec=eac3,ac3,truehd,dts,aac`.
- **`res`**: resolución de vídeo preferida. Ejemplo: `res=1080p`.
- **`read_mode`**: política de lectura. Valores: `block` (default), `timeout`, `nonblock`.
- **`read_timeout_ms`**: timeout para `read_mode=timeout`. Default: 1000.
- **`mountpoint`**: punto de montaje. Default: `/mnt/videofuser/`.
- **`fuse_direct_io`**: booleano. Default: `false`. Si es `true`, FUSE se monta con la opción `direct_io` activada, lo que evita el page cache del kernel para los archivos del MKV virtual. Útil si el reproductor usa `mmap()` para leer el archivo (algunos reproductores lo hacen, especialmente en sistemas con poca RAM o con archivos muy grandes), porque `inval_inode` no afecta a las lecturas vía mmap si el page cache está activo. Activar `direct_io` reduce el rendimiento general (cada `read()` va al FUSE sin caching) pero garantiza que las invalidaciones surtan efecto.
- **`fuse_kernel_cache`**: booleano. Default: `true`. Si es `false`, FUSE indica al kernel que no cachee páginas del MKV virtual (opción de montaje opuesta a `kernel_cache`). En la mayoría de casos `true` es mejor (reproductor lee páginas cacheadas sin overhead). Se desactiva solo en escenarios de debugging o cuando se observa comportamiento inconsistente con invalidaciones.

Las dos listas jerárquicas (`audio_langs` y `sub_langs`) son completamente independientes. El usuario puede tener `audio_langs=it,fr,de` y `sub_langs=es,en,cs` perfectamente.

### 11.3 Componentes internos

El demonio se compone de los siguientes componentes funcionales:

- **Watcher**: componente asíncrono que monitoriza la lista de torrents del cliente del usuario. Polling con intervalo configurable (default 5 segundos) o subscripción a eventos del cliente si la API lo soporta. Por cada torrent detectado, comprueba si tiene la estructura del sistema (existencia del archivo `info/<base>_binstruct_<vv>.ebml`). Si no la tiene, lo ignora. Si la tiene y no estaba registrado, lo añade al Registry, ordena la descarga prioritaria del binstruct y manifest, espera a que estén descargados, los procesa, y registra el torrent como activo.

- **Registry**: mantiene un mapa `HashMap<TorrentId, MountedTorrent>` donde cada `MountedTorrent` contiene: el binstruct cargado dentro de un `Arc`, la VFR cache (`HashMap<TrackId, Arc<VfrFile>>` con carga perezosa), las IntervalMaps de descarga por pista (`HashMap<TrackId, Arc<RwLock<IntervalMap>>>`), las prefs aplicadas (`PrefsSnapshot`), el filtro resuelto activo (lista de TrackIds incluidos), y la tabla de inodos del FUSE (`BiMap<Inode, FileKey>`). El Registry se accede desde múltiples threads y se protege con `RwLock` para mutaciones (añadir/quitar torrent), con datos por torrent detrás de `Arc` propio para acceso sin contención.

- **Muxer (lib `videofuser-muxer`)**: componente stateless que, dado un binstruct, una VFR cache, un IntervalMap de descarga, acceso a archivos crudos, un filtro de TrackIds y un rango virtual `(offset, length)`, produce los bytes del MKV virtual correspondientes. Aplica los modos `raw` (audio) o `transform` (vídeo) según la pista. Los detalles de su algoritmo están en el capítulo 13.

- **Fuser (lib `videofuser-fs`)**: implementa el trait `Filesystem` de la crate `fuser`. Multiplexa todos los torrents bajo un único mountpoint. Por cada `read()` recibido, identifica el inodo correspondiente y delega al muxer (si es un MKV virtual) o al disco (si es un sidecar de subs). Mantiene el árbol de directorios con un subdirectorio por torrent activo, dentro del cual está el MKV virtual y los sidecars visibles según filtro y disponibilidad.

- **IPC server**: listener en el socket Unix `/run/user/<uid>/videofuser.sock`. Recibe mensajes en formato MessagePack y los procesa según el protocolo del capítulo 18. Cada conexión es de corta duración; el cliente envía un comando, el servidor responde, y la conexión se cierra.

- **Track controller** (fase posterior): observa los `read()` del FUSE para inferir patrones de acceso. Cuando detecta lectura secuencial (streaming), pre-prioriza piezas del torrent. Cuando detecta seek, ajusta. Comunica las prioridades al cliente torrent vía su API. En el MVP no está implementado; las descargas siguen las prioridades por defecto del cliente torrent.

- **Preferences store**: almacena las prefs del usuario en un archivo de configuración (`~/.config/videofuser/prefs.toml`). Los cambios mediante `videofuser prefs set` modifican el archivo y aplican los cambios in-memory. Las prefs incluyen las dos listas jerárquicas (audio y subs) y el resto de claves del capítulo 11.2.

- **Filter resolver (lib `videofuser-filter`)**: componente que determina, a partir de un binstruct y unas prefs, qué TrackIds se incluyen en el MKV virtual y qué sidecars se exponen. Es el cerebro del filtrado. Su lógica completa está en el capítulo 12 y 15.

### 11.4 Estructura del mountpoint

```
/mnt/videofuser/
├── <torrent_id>/
│   ├── <base>.mkv               (virtual, generado on-the-fly según prefs actuales)
│   ├── <base>.es.0.srt          (sidecars solo si idioma matchea sub_langs y archivo descargado)
│   ├── <base>.en.0.srt
│   └── ...
```

Solo se exponen los sidecars completamente descargados Y cuyo idioma esté en la lista filtrada según prefs.

---

## 12. Comportamiento en runtime

### 12.1 Resolución del filtro de pistas (audio)

Dada la configuración del usuario `audio_langs = [L1, L2, L3, L4, L5]` (lista de hasta 5 elementos) y el `OriginalLanguage = O` del binstruct, el filtro de audio se resuelve así:

1. Construye el conjunto candidato:
   - Todas las TrackPolicy con `CodecType=audio` cuyo `LanguageCode` esté en `audio_langs`.
   - Más todas las TrackPolicy con `CodecType=audio` cuyo `LanguageCode == O` (el idioma original), si no estaban ya incluidas.
2. Si el conjunto candidato es **vacío** (ningún match con `audio_langs` y el idioma original tampoco está disponible — caso muy raro):
   - Fallback: incluir **todas las pistas de audio del binstruct**.
3. El conjunto resultante se usa para regenerar el Tracks element del MKV virtual y para decidir qué Blocks se emiten en los Clusters.

**Nota sobre el original**: el idioma original siempre se incluye salvo que el conjunto resultante deba ser vacío, lo cual no ocurre nunca porque si el original existe, está incluido. El fallback "todas las pistas" solo se activa si el binstruct no tiene ninguna pista en ninguno de los idiomas preferidos NI en el original — situación que solo se da si el publicador no respetó el requisito de FlagDefault=1, lo cual debería haber sido detectado en `binstruct gen`.

### 12.2 Resolución del filtro de subtítulos

Dada `sub_langs = [S1, S2, S3, S4, S5]`:

1. Construye el conjunto candidato: todos los archivos de sidecar de subs cuyo `<lang>` (extraído del nombre de archivo) esté en `sub_langs`.
2. Si vacío: fallback a **todos los sidecars de subs disponibles**.
3. El conjunto resultante define qué sidecars se exponen en el directorio del MKV virtual.

Los subtítulos no tienen concepto de "original"; el filtro es puramente por idiomas preferidos del usuario.

### 12.3 FlagDefault del MKV virtual

En el Tracks element regenerado por el muxer, exactamente una pista de audio debe tener `FlagDefault=1` (las demás se patchan a 0). La selección sigue esta lógica:

- Si las prefs `audio_langs` matchean: la pista del idioma de mayor prioridad (`L1` si está disponible, si no `L2`, etc.) y dentro de ese idioma, la pista con el mejor códec según `audio_codec`.
- Si las prefs no matchean y el original está disponible: la pista del idioma original (con el mejor códec disponible si hay variantes).
- Si fallback "todas las pistas": la pista del idioma original (que siempre estará disponible si el publicador respetó el requisito).

Este patch se aplica sobre las TrackEntry blobs en runtime, modificando exactamente el byte correspondiente al elemento `FlagDefault` (UInt de 1 byte) sin cambiar el tamaño total de la TrackEntry.

### 12.4 Reads y reconstrucción del MKV virtual

Cada `read(inode, offset, length)` que recibe el FUSE se procesa así:

1. **Resolución de inodo**: el Fuser consulta el `inode_table` del torrent correspondiente para identificar qué archivo se está leyendo.

2. **Caso A: sidecar de subtítulo**. Lectura directa del archivo en disco. Los sidecars expuestos en el directorio están siempre completamente descargados (capítulo 12.8), por lo que la lectura es directa y no requiere bloqueo.

3. **Caso B: MKV virtual**. Delegación al muxer. El muxer realiza el siguiente proceso:
   a. Mapea el rango virtual `[offset, offset+length)` a una secuencia de operaciones según la estructura del MKV virtual con el filtro activo:
      - Leer bytes del PreTracksBlob (en RAM, siempre disponibles).
      - Leer bytes del Tracks element regenerado (construido en RAM al cargar el filtro).
      - Leer bytes del PostTracksBlob (en RAM).
      - Leer rangos de archivos crudos de pistas, aplicando modo `raw` o `transform` según corresponda.
      - Generar bytes de cabeceras de Cluster, SimpleBlock, Cues y SeekHead según el algoritmo determinista del capítulo 13.
   b. Ejecuta cada operación, recolectando bytes.
   c. Devuelve el buffer concatenado.

### 12.5 Política de bloqueo y timeouts

Cuando una operación del muxer requiere bytes de un archivo crudo que aún no está descargado en el rango solicitado, entra en juego la política de lectura, configurable mediante la pref `read_mode`.

#### `block` (default)

El thread del FUSE bloquea esperando una notificación de que las piezas necesarias han llegado. La notificación viene del componente que monitoriza el progreso del cliente torrent.

Cuando llega la notificación:

1. Se vuelve a consultar el `download_state` del rango.
2. Si está completo, se lee del disco y se devuelven los bytes.
3. Si aún incompleto, se sigue esperando.

Esta política garantiza que el reproductor nunca recibe ceros: solo recibe pausa breve. Si el cliente torrent no está descargando (sin peers, etc.), el read se bloquea indefinidamente; el reproductor pausará la reproducción.

#### `timeout`

Igual que `block` pero con un límite temporal en `read_timeout_ms`. Al expirar el timeout:

1. Los bytes pendientes se rellenan con ceros, alineados a frame completo.
2. Se devuelve el buffer (mezcla de bytes reales y ceros) al reproductor.

El receptor invalida el caché del kernel con `Notifier::inval_inode` cuando posteriormente lleguen los datos reales, de modo que las siguientes lecturas vean el contenido correcto. Esto solo invalida el caché del kernel, no el caché interno del reproductor; si el reproductor cacheó los ceros en su buffer demuxer, puede continuar mostrándolos hasta que el usuario haga seek o reabra el archivo. Por eso esta política se considera menos robusta que `block`.

#### `nonblock`

Sin bloqueo. Si los bytes no están disponibles, se devuelven ceros alineados a frame inmediatamente. Política más permisiva pero con riesgo de corrupción visible más alto. Útil para casos donde el reproductor no tolera lecturas largas.

### 12.6 Alineación a frame completo

En cualquier política que rellene con ceros (timeout o nonblock), la alineación es **por frame completo**, no por byte. Si la zona descargada se corta en medio de un frame, ese frame entero se considera no disponible y se rellena con ceros.

Razones:

- Los códecs detectan frames inválidos y los descartan limpiamente. Un frame con todos sus bytes a cero suele rechazarse sin propagación de errores.
- Un frame parcialmente válido (bytes inicio-medio reales, bytes medio-fin a cero) puede ser malinterpretado por el códec como un frame válido pero con contenido raro, generando artefactos visibles más feos que un frame nulo.

Para detectar el "último frame completamente descargado" de una pista, el muxer:

1. Consulta el `download_state` (IntervalMap) de la pista para obtener los rangos descargados del archivo crudo.
2. Por cada rango contiguo `[start, end)`, busca el frame K más alto cuyo `frame_offset + frame_size <= end`. Ese frame es el último completamente descargado en ese rango contiguo.
3. Frames fuera de los rangos contiguos descargados se consideran no disponibles.

Para CBR, `frame_offset = K * frame_size` (del TrackPolicy). Para VBR, se calcula con suma acumulada del VFR.

### 12.7 Cambios de preferencias

Cuando el usuario cambia prefs:

- El demonio recalcula el filtro para todos los torrents activos.
- El conjunto de TrackIds en cada MKV virtual cambia. Esto significa que **el contenido bytes del MKV virtual cambia completamente**.
- El demonio actualiza la mtime del inode del MKV virtual e invalida el caché del kernel con `inval_inode` sobre todo el archivo.
- El reproductor en curso sigue con la versión que tenía abierta. Para ver los cambios debe cerrar y reabrir el archivo.
- Los sidecars de subs visibles cambian: el demonio llama a `inval_entry` para cada sidecar que aparezca o desaparezca.
- Las prioridades de descarga del cliente torrent se recalculan (fase posterior).

### 12.8 Política para sidecars no descargados

Un archivo de sidecar de subs **no se expone en el directorio del MKV virtual hasta que está completamente descargado**. Mientras está parcialmente descargado, no aparece en `readdir`, no es accesible vía `lookup`, y `open()` retorna ENOENT.

Cuando una pieza completa la descarga del sidecar:

1. El demonio verifica el hash SHA-256 contra el declarado en el manifest.
2. Si el hash es correcto: llama a `inval_entry(parent_inode, name)` para que el reproductor lo descubra en su siguiente `readdir`.
3. Si el hash es incorrecto: marca el archivo como corrupto, ordena re-descarga, no expone.

Esto evita que el reproductor intente abrir un sidecar parcial y muestre subtítulos truncados o corruptos.

### 12.9 Notificaciones de cambio de estado

- `Notifier::inval_inode(mkv_inode, virtual_offset_start, length)`: cuando llegan piezas que completan rangos del MKV virtual.
- `Notifier::inval_entry(parent_inode, name)`: cuando un sub completa descarga, cambia visibilidad por prefs, o cualquier cambio que afecte a la lista de archivos expuestos.

---

## 13. El muxer

### 13.1 Arquitectura interna por capas

El muxer es el componente con mayor concentración de complejidad del sistema. Para mantenerlo manejable, su implementación se organiza internamente en **cuatro capas** con responsabilidades estrictamente separadas. Cada capa tiene una interfaz clara y se puede testear de forma aislada.

#### Capa 1: Layout planner

Toma como entrada un binstruct cargado y un filtro de TrackIds, y produce el **mapa de layout virtual del MKV virtual completo**: por cada rango de bytes virtuales, qué fuente proporciona los datos.

El layout es una secuencia ordenada de `LayoutSection`, cada una de un tipo:

```rust
enum LayoutSection {
    PreTracksBlob,
    TracksElement { entries: Vec<TrackEntryPatch> },
    PostTracksBlob,
    ClusterHeader { cluster_index: u32, timestamp: u64 },
    SimpleBlock { cluster_index: u32, track_id: u32, frame_index: u32 },
    Cues,
    SeekHead,
}
```

Cada sección tiene un offset virtual de inicio y una longitud calculable a priori. El layout planner produce un array de `(virtual_offset_start, length, LayoutSection)` que sirve como índice para el resto del muxer.

Esta capa es **pura y determinista**: dadas las mismas entradas, produce el mismo layout. No hace I/O, no consulta el estado de descarga, no convierte códecs.

#### Capa 2: Track materializer

Dado un `LayoutSection` que requiere bytes de pista (típicamente un `SimpleBlock`), produce los bytes correspondientes consultando el archivo crudo de la pista.

Para CBR audio: lee directamente con `frame_offset = frame_index × CbrFrameSize`. Para VBR: consulta el VFR para encontrar el offset.

Esta capa **sí hace I/O** sobre los archivos crudos descargados. Consulta el `download_state` para verificar disponibilidad y aplica la política de bloqueo/timeout/nonblock.

#### Capa 3: Codec transformer

Transforma los bytes crudos producidos por la capa 2 al formato esperado por el contenedor MKV. Para audio, es la identidad (modo `raw`). Para vídeo, aplica conversión Annex B → AVCC consultando la tabla NalLengths del VFR (modo `transform`).

Esta capa es **pura**: dado un input idéntico produce un output idéntico. No hace I/O, no maneja estado.

#### Capa 4: Read coordinator

Es el punto de entrada externo del muxer. Recibe un `read(virtual_offset, length)` desde el FUSE y orquesta las capas anteriores para producir los bytes solicitados:

1. Consulta el layout planner (capa 1) para identificar qué `LayoutSection` cubren el rango solicitado.
2. Para cada sección:
   - Si es bytes de skeleton (PreTracksBlob, PostTracksBlob, Cues, SeekHead), los obtiene de RAM/mmap directamente.
   - Si es TracksElement, aplica patches in-memory de FlagDefault y emite los bytes resultantes.
   - Si es ClusterHeader, los genera sintéticamente.
   - Si es SimpleBlock, invoca a la capa 2 (track materializer) para obtener bytes crudos, luego invoca a la capa 3 (codec transformer) si la pista requiere transformación, y construye el SimpleBlock completo (cabecera EBML + payload transformado).
3. Concatena los bytes de todas las secciones cubiertas y devuelve el rango exacto solicitado.

Esta capa **maneja la concurrencia** (waiters, condvars), las invalidaciones de caché y las políticas de timeout. Es la única que tiene estado mutable.

#### Beneficios de la separación

- **Testabilidad**: cada capa se puede testear con mocks de las inferiores.
- **Debugging**: si los bytes producidos son incorrectos, el problema está en una capa específica identificable por los inputs/outputs intermedios.
- **Performance profiling**: cada capa se puede instrumentar por separado para identificar cuellos de botella.
- **Refactoring**: cambios en el formato de los archivos crudos solo afectan a la capa 2; cambios en el formato del MKV solo afectan a la capa 1.

Las capas 1 y 3 son completamente puras (sin I/O, sin estado), lo que las hace especialmente fáciles de testear con casos sintéticos.

### 13.2 Reconstrucción determinista filtrada

El muxer es determinista dado un binstruct, un conjunto de archivos crudos y un **filtro de TrackIds**. Con los mismos inputs produce siempre los mismos bytes.

### 13.3 Estructura del MKV virtual

```
[EBML Header] (verbatim de PreTracksBlob)
[Segment Header]
  [Info element] (verbatim de PreTracksBlob)
  [Tracks element] (regenerado: subset de TrackEntries según filtro)
  [Chapters / Attachments / Tags] (verbatim de PostTracksBlob, si existe)
  [Cluster_0]
  [Cluster_1]
  ...
  [Cluster_M-1]
  [Cues] (regenerados)
  [SeekHead] (regenerado, al final del Segment)
```

### 13.4 Generación del Tracks element

El muxer regenera el Tracks element on-the-fly:

1. Determina el filtro de TrackIds según prefs.
2. Construye la lista ordenada de TrackEntry blobs correspondientes (en el orden en que aparecían en el binstruct).
3. Aplica patches in-place a las TrackEntry blobs:
   - `FlagDefault=1` para la pista seleccionada según 12.3, `FlagDefault=0` para las demás.
   - El patch modifica un único byte por TrackEntry sin cambiar el tamaño total.
4. Calcula el tamaño total del Tracks element como cabecera EBML + suma de tamaños de TrackEntry blobs.
5. Emite el Tracks element: ID `0x1654AE6B` + Size + concatenación de TrackEntry blobs.

### 13.5 Generación de Clusters

Para cada cluster N:

1. Obtener `cluster_timestamp_n` del binstruct.
2. Emitir cabecera de Cluster.
3. Para cada track en el filtro (en orden ascendente de TrackId):
   - Computar el rango de frames de la pista que caen en `[cluster_timestamp_n, cluster_timestamp_next)`.
   - Para cada frame: emitir un SimpleBlock.
4. Cerrar Cluster.

Los TrackIds que no están en el filtro **no emiten Blocks** en ningún cluster.

### 13.6 Modos del muxer: raw vs transform

El muxer opera en uno de dos modos según el tipo de pista:

**Modo `raw`** (audio):
- El payload del Block se obtiene leyendo directamente del archivo crudo en `[frame_offset, frame_offset + frame_size)`.
- Sin transformación.

**Modo `transform`** (vídeo):
- El payload del Block se construye convirtiendo el frame del archivo crudo (formato Annex B) a formato AVCC/HVCC:
  1. Leer el frame del archivo crudo en `[frame_offset, frame_offset + frame_size)`.
  2. Para cada NAL unit del frame (cantidad indicada por `nal_count`, longitudes individuales en NalLengths):
     - Saltarse el start code (4 bytes `0x00000001` o 3 bytes `0x000001`).
     - Leer los siguientes `nal_length` bytes (el NAL data).
  3. Construir el payload AVCC: por cada NAL unit, prepender 4 bytes con la longitud (u32 big-endian), seguido de los bytes del NAL data, todos concatenados.
  4. El tamaño del payload AVCC es `4 * nal_count + sum(nal_lengths)`, donde `nal_lengths` se obtiene del VFR.

El muxer determina el modo a aplicar consultando el `CodecType` de cada TrackPolicy: `video` → transform, `audio` → raw. La lógica de transform es codec-agnóstica respecto al subtipo concreto (H.264 vs H.265 vs AV1) porque opera solo sobre la estructura genérica de NAL units / OBUs.

### 13.7 Regeneración de Cues

Tras emitir todos los Clusters, el muxer construye la sección Cues. Por cada keyframe de cada pista de vídeo en el filtro:

- `CueTime` = timestamp del frame keyframe.
- `CueTrackPositions`: trackId + clusterPosition.

Los offsets de cluster se calculan durante la emisión y se almacenan en una tabla `cluster_offsets[]`.

### 13.8 Regeneración de SeekHead

El muxer emite un SeekHead **al final del Segment**, después de los Cues. Matroska permite SeekHead tanto al principio como al final; los reproductores buscan ambos. Emitirlo al final evita el problema de tener que conocer offsets antes de emitirlos.

El SeekHead contiene punteros a:
- Info element
- Tracks element
- Chapters (si existe)
- Attachments (si existe)
- Tags (si existe)
- Cues element

Los offsets se calculan durante la emisión.

### 13.9 Manejo de pistas no descargadas o sin VFR

Si una pista en el filtro tiene su VFR aún no descargado:

- El muxer la **excluye temporalmente del filtro**: no emite su TrackEntry en el Tracks element ni sus Blocks en los Clusters. Es como si no estuviera en el filtro.
- Cuando el VFR se descarga, el muxer recarga su estado y la incluye. Invalida toda la región de Clusters afectada con `inval_inode` y emite `inval_inode` sobre el Tracks element para que se relea (aunque el reproductor en curso no lo hará; el cambio será visible en futuras aperturas).

Si una pista en el filtro tiene VFR descargado pero archivo crudo no descargado:

- Los Blocks se emiten con payload de ceros del tamaño correcto, según la política de read.

### 13.10 Tamaño total del MKV virtual

Calculable al cargar el binstruct + filtro:

```
total_size = pre_tracks_blob_size
           + tracks_element_size (tamaño de la cabecera EBML + suma de TrackEntry blobs filtradas)
           + post_tracks_blob_size
           + sum(cluster_size(n) for n in 0..M)
           + cues_size
           + seekhead_size
```

---

## 14. Concurrencia y estado

### 14.1 Modelo de concurrencia

El demonio `videofuser` usa dos planos de concurrencia distintos.

#### Control plane (tokio)

Watcher, IPC server y comunicación con el cliente torrent corren dentro de un único runtime tokio. Estos componentes son async y comparten el mismo runtime para minimizar overhead. Se ejecutan típicamente con un pool de threads pequeño (2-4) ya que las operaciones son mayoritariamente I/O ligero.

#### Data plane (threads de fuser)

La crate `fuser` arranca su propio pool de threads para servir requests del FUSE. Cada `read()` se procesa en un thread del pool y puede bloquearse independientemente esperando piezas del torrent. Estos threads no están dentro de tokio; son threads del sistema gestionados directamente por `fuser`.

La separación es importante: bloqueos en el data plane no afectan al control plane. El IPC sigue respondiendo aunque haya muchos `read()` bloqueados esperando datos. El watcher sigue detectando torrents nuevos. Las prefs siguen pudiendo cambiarse.

### 14.2 Estado compartido

#### Por torrent

- **`binstruct: Arc<Binstruct>`**: inmutable después de la carga inicial. Lectura libre de lock vía el `Arc`. El acceso a las secciones internas (PreTracksBlob, TrackEntries, etc.) se hace por mmap.
- **`vfr_cache: Arc<RwLock<HashMap<TrackId, Arc<VfrFile>>>>`**: carga perezosa. Lock de escritura solo para insertar; lecturas vía `Arc` clonado.
- **`download_state: HashMap<TrackId, Arc<RwLock<IntervalMap>>>`**: estructura por pista, actualizada por el track controller cuando llegan piezas. Lectura por el muxer en cada operación que toque la pista.
- **`prefs_applied: Arc<ArcSwap<PrefsSnapshot>>`**: snapshot de las prefs activas. Updates lock-free vía `arc-swap`. Cambios disparan recálculo del filtro.
- **`active_filter: Arc<ArcSwap<TrackFilter>>`**: lista de TrackIds incluidos en el MKV virtual con las prefs actuales. Recalculado cuando cambian las prefs.

#### Global

- **`registry: Arc<RwLock<Registry>>`**: lock de escritura solo para añadir/quitar torrent (operación rara). Lectura por todos los componentes.

### 14.3 Sincronización de bloqueo en reads

Cuando un thread del FUSE bloquea esperando piezas, se registra en una estructura de espera por pista:

```rust
struct WaitGroup {
    waiters: Mutex<Vec<Waiter>>,
}

struct Waiter {
    range: Range<u64>,
    notify: Condvar,
}
```

Cuando el track controller recibe notificación del cliente torrent de que una pieza llegó:

1. Identifica qué archivos crudos están afectados (qué pistas).
2. Actualiza el `download_state` (IntervalMap) atómicamente.
3. Notifica a todos los waiters de las `WaitGroup` afectadas.
4. Llama a `Notifier::inval_inode` del FUSE para invalidar el caché del kernel en los rangos virtuales correspondientes.

Para evitar overhead de fine-grained tracking, las `WaitGroup` se mantienen por (torrent_id, track_id), no por rango. El track controller hace coarse-grained notification y los waiters re-evalúan su rango al despertarse.

### 14.4 Atomicidad de cambios de filtro

Cuando cambian las prefs y el filtro debe recalcularse:

1. El componente que recibe el cambio (IPC handler) llama al filter resolver con las nuevas prefs.
2. El filter resolver produce un nuevo `TrackFilter`.
3. El nuevo filtro se publica vía `arc-swap` sobre `active_filter`. Esta operación es atómica.
4. Las nuevas lecturas del FUSE leen el filtro nuevo.
5. Las lecturas en curso siguen con el filtro antiguo hasta completarse (el `Arc` viejo no se libera mientras hay readers).
6. El demonio invalida el caché del kernel del MKV virtual y notifica cambios de visibilidad de sidecars.

### 14.5 Apagado limpio

Al recibir `SIGTERM` o el comando `videofuser shutdown`:

1. El IPC server deja de aceptar nuevas conexiones.
2. El watcher se para.
3. Se notifica a todos los waiters bloqueados con un flag de "shutting down" para que liberen los reads con error EIO o equivalente.
4. Se desmonta el FUSE.
5. Se cierra el socket Unix y se libera el lockfile.
6. El proceso termina con código 0.

---

## 15. Algoritmos clave

### 15.1 Resolución del filtro de audio

```
fn resolve_audio_filter(binstruct: &Binstruct, prefs: &Prefs) -> Vec<TrackId> {
    let mut included = HashSet::new();

    for tp in binstruct.track_policies.iter().filter(|t| t.codec_type == Audio) {
        if prefs.audio_langs.contains(&tp.language_code) || tp.language_code == binstruct.source.original_language {
            included.insert(tp.track_id);
        }
    }

    if included.is_empty() {
        // Fallback: todas las pistas de audio
        for tp in binstruct.track_policies.iter().filter(|t| t.codec_type == Audio) {
            included.insert(tp.track_id);
        }
    }

    // Vídeo: todas las 8 resoluciones (no se filtran por idioma; res se filtra en otra capa)
    for tp in binstruct.track_policies.iter().filter(|t| t.codec_type == Video) {
        included.insert(tp.track_id);
    }

    let mut result: Vec<_> = included.into_iter().collect();
    result.sort();
    result
}
```

### 15.2 Resolución del filtro de subtítulos

```
fn resolve_subs_filter(sub_files: &[SubFile], prefs: &Prefs) -> Vec<&SubFile> {
    let candidate: Vec<_> = sub_files.iter()
        .filter(|s| prefs.sub_langs.contains(&s.language_code))
        .collect();

    if candidate.is_empty() {
        sub_files.iter().collect()
    } else {
        candidate
    }
}
```

### 15.3 Selección de pista por defecto en MKV virtual

```
fn select_default_audio(filtered_tracks: &[TrackPolicy], prefs: &Prefs, binstruct: &Binstruct) -> TrackId {
    // Buscar la primera pista de las prefs disponible
    for lang in &prefs.audio_langs {
        let tracks_in_lang: Vec<_> = filtered_tracks.iter()
            .filter(|t| &t.language_code == lang && t.codec_type == Audio)
            .collect();

        if !tracks_in_lang.is_empty() {
            return select_best_codec(&tracks_in_lang, &prefs.audio_codec).track_id;
        }
    }

    // Fallback: pista original
    binstruct.source.original_default_track_id
}
```

### 15.4 Mapping virtual_offset → fuente

Dado un offset virtual `O` en el MKV virtual, determinar qué bytes generar:

1. Si `O < pre_tracks_blob_size`: leer bytes del PreTracksBlob en RAM, en posición `O`.
2. Si `O < pre_tracks_blob_size + tracks_element_size`: leer bytes del Tracks element regenerado (construido en RAM al cargar el filtro, con FlagDefault patches aplicados) en posición `O - pre_tracks_blob_size`.
3. Si `O < pre_tracks_blob_size + tracks_element_size + post_tracks_blob_size`: leer bytes del PostTracksBlob.
4. Si `O < cues_offset`: identificar qué Cluster contiene el byte `O`. Búsqueda binaria en `cluster_offsets[]` para encontrar el cluster N tal que `cluster_offsets[n] <= O < cluster_offsets[n+1]`. Dentro del cluster, identificar qué sección (cabecera, SimpleBlock K de qué pista, etc.) corresponde a `O - cluster_offsets[n]`.
5. Si `O < seekhead_offset`: leer bytes de la sección Cues regenerada (también construida en RAM).
6. Si `O >= seekhead_offset`: leer bytes del SeekHead regenerado.

El array `cluster_offsets[]` se precomputa al cargar el binstruct y al determinar el filtro. Tiene una entrada por cluster, indicando el offset virtual del inicio de cada cluster.

### 15.5 Búsqueda de frame por timestamp

Para encontrar el frame de la pista T cuyo timestamp >= ts:

#### CBR audio

Cálculo directo: `frame_index = ceil(ts / frame_duration)` (con `frame_duration` del TrackPolicy en track timebase units).

#### VBR (todo vídeo, audio variable)

Búsqueda en el índice acumulado precomputado al cargar el VFR.

El índice acumulado es un array `accumulated_time[]` con una entrada cada N frames (típicamente N=1024). Cada entrada `accumulated_time[K]` contiene la suma `sum(frame_durations[0..K*N])`.

Algoritmo:

1. Búsqueda binaria en `accumulated_time[]` para encontrar el rango `[K*N, (K+1)*N)` que contiene el timestamp objetivo.
2. Recorrido lineal dentro del bloque de N frames para refinar al frame exacto.

Tiempo total: O(log(num_frames / N) + N) = O(log(num_frames / 1024) + 1024).

### 15.6 Alineación a frame completo en datos parciales

Dado un rango `[start, end)` solicitado del archivo crudo y el conjunto de rangos descargados de la pista (consultado del IntervalMap):

1. Encontrar la intersección entre `[start, end)` y los rangos descargados.
2. Por cada rango descargado contiguo `[d_start, d_end)`:
   a. Buscar el primer frame que empieza en o después de `d_start`.
   b. Buscar el último frame que termina en o antes de `d_end`.
   c. El subrango "alineado a frame" dentro de `[d_start, d_end)` es `[first_frame_offset, last_frame_offset + last_frame_size)`.
3. Concatenar los subrangos alineados.

Para CBR, los frame_offsets se calculan directamente. Para VBR, se usa el índice acumulado del VFR.

### 15.7 Notificaciones cross-thread

Cuando una pieza del torrent llega:

1. El track controller (o, en MVP, el watcher con polling) recibe la notificación del cliente torrent.
2. Identifica qué archivos crudos están afectados consultando el mapping de archivos a piezas del .torrent.
3. Por cada pista afectada:
   a. Actualiza el `download_state` (IntervalMap) con el nuevo rango disponible. Operación protegida por `RwLock`.
   b. Notifica los `Condvar` de los waiters registrados en la `WaitGroup` correspondiente.
4. En paralelo, calcula los rangos virtuales del MKV virtual afectados por la nueva descarga (translación de rangos de archivo crudo a rangos virtuales mediante el binstruct y el filtro activo).
5. Llama a `Notifier::inval_inode(mkv_inode, virtual_offset_start, length)` para cada rango virtual afectado.

El paso 4 es no trivial porque un rango de archivo crudo puede mapear a múltiples rangos virtuales no contiguos (si los frames de la pista están dispersos por varios clusters). Una optimización razonable es invalidar el MKV virtual entero cuando llegan piezas significativas; esto no causa problemas funcionales (solo un pequeño overhead de re-lectura por el kernel).

---

## 16. Casos límite y políticas

### 16.1 Reinicio del demonio con datos parcialmente descargados

Al arrancar, el demonio reconstruye las IntervalMaps escaneando archivos en disco o consultando al cliente torrent.

### 16.2 Cierre del cliente torrent

El demonio sigue funcionando con los datos ya descargados.

### 16.3 Seek aleatorio del reproductor

El muxer responde según el algoritmo de mapping. Si cae en zona no descargada, aplica política de read.

### 16.4 Cambio de pista mid-playback

El reproductor empieza a leer offsets correspondientes a Blocks de la nueva pista. El muxer responde según disponibilidad. La nueva pista debe estar en el filtro actual; si no, no es seleccionable porque no está en el Tracks element.

### 16.5 Cambio de prefs mid-playback

El contenido del MKV virtual cambia. La reproducción en curso continúa con la versión antigua. Para ver los cambios, el usuario cierra y reabre el archivo.

### 16.6 Idioma original ausente en torrent

No debería ocurrir si el publicador respetó el requisito de FlagDefault=1. Si ocurre (binstruct corrupto, etc.), el muxer entra en modo fallback "todas las pistas".

### 16.7 Idioma preferido ausente en torrent (caso normal)

Si las prefs `audio_langs` no tienen ningún match en el torrent, el filtro queda con solo el idioma original. El usuario ve una sola pista de audio (la original). Esto es correcto: el contenido no tiene lo que el usuario quiere.

### 16.8 Listas de prefs vacías

Si el usuario establece `audio_langs=` (vacía), el filtro entra en modo fallback automáticamente y se incluyen todas las pistas. Equivalente a desactivar el filtro.

### 16.9 Colisiones de nombre entre subs

Resolución con sufijos `<idx>` numéricos.

### 16.10 Corrupción de archivos

Verificación de hashes SHA-256 al completar cada archivo. En caso de mismatch, se ordena re-descarga.

### 16.11 Versiones de archivo

Si dos archivos coinciden excepto en `<vv>`, prevalece la versión más alta.

### 16.12 Sidecar parcialmente descargado

Oculto del directorio (capítulo 12.8). No accesible hasta completarse.

### 16.13 Tamaño del Tracks element con filtro

Con el filtro típico (audio_langs de 5 idiomas + original + 8 vídeos), el Tracks element resultante contiene del orden de 15-25 TrackEntry. Tamaño en KB. Cómodo para todos los reproductores.

### 16.14 Unicode en nombres

Preservado tal cual en sistemas Linux con locale UTF-8.

---

## 17. Binarios y crates del workspace

### 17.1 Workspace Rust

```
videofuser/
├── Cargo.toml
├── crates/
│   ├── videofuser-binstruct/       (lib: schema y serialización del binstruct)
│   ├── videofuser-vfr/             (lib: schema y serialización VFR)
│   ├── videofuser-muxer/           (lib: reconstrucción MKV virtual con filtro y modos raw/transform)
│   ├── videofuser-fs/              (lib: FUSE multiplexor)
│   ├── videofuser-ipc/             (lib: protocolo IPC)
│   ├── videofuser-parser-common/   (lib: contrato común de parsers)
│   ├── videofuser-manifest/        (lib: parsing y generación manifests)
│   └── videofuser-filter/          (lib: filter resolver para audio y subs)
├── bins/
│   ├── videofuser/                 (bin: demonio + cliente CLI)
│   ├── binstruct/                  (bin: gen, inspect, verify)
│   └── parser-*/                   (bins por códec)
└── tests/
    └── integration/
```

### 17.2 Dependencias externas clave

| Crate | Uso |
|---|---|
| `fuser` | FUSE filesystem |
| `ebml-iterable` | Parser EBML del MKV |
| `byteorder` | Lectura/escritura binaria |
| `memmap2` | mmap del binstruct |
| `tokio` | Runtime async |
| `arc-swap` | Updates lock-free |
| `crossbeam` | Sincronización |
| `zstd` | Compresión |
| `serde` + `rmp-serde` | Serialización IPC |
| `clap` | CLI parsing |
| `tracing` | Logging |
| `interval-tree` | IntervalMap |
| `sha2` | Hashes |

### 17.3 Trait TorrentClientAdapter

El sistema se desacopla del cliente torrent concreto mediante el trait `TorrentClientAdapter`. Cada cliente soportado (qBittorrent, Transmission, Deluge, rTorrent, libtorrent embebido, etc.) tiene su propia implementación.

Definición conceptual del trait (la implementación concreta puede variar en detalles de tipos):

```rust
#[async_trait]
pub trait TorrentClientAdapter: Send + Sync {
    /// Lista todos los torrents activos en el cliente.
    async fn list_torrents(&self) -> Result<Vec<TorrentInfo>, AdapterError>;

    /// Obtiene los archivos de un torrent específico, con su path relativo,
    /// tamaño total y rangos de bytes ya descargados.
    async fn list_files(&self, torrent_id: &TorrentId) -> Result<Vec<TorrentFileInfo>, AdapterError>;

    /// Obtiene el estado de descarga de un archivo específico, devolviendo
    /// los rangos de bytes ya disponibles en disco.
    async fn file_download_state(
        &self,
        torrent_id: &TorrentId,
        file_path: &str,
    ) -> Result<IntervalMap<u64>, AdapterError>;

    /// Establece prioridad de descarga a nivel de archivo.
    async fn set_file_priority(
        &self,
        torrent_id: &TorrentId,
        file_path: &str,
        priority: FilePriority,
    ) -> Result<(), AdapterError>;

    /// Establece prioridad a nivel de pieza, si el cliente lo soporta.
    /// Devuelve `Err(AdapterError::Unsupported)` si no.
    async fn set_piece_priority(
        &self,
        torrent_id: &TorrentId,
        piece_indices: &[u32],
        priority: PiecePriority,
    ) -> Result<(), AdapterError>;

    /// Subscribe a notificaciones de piezas completadas. Devuelve un Stream
    /// de eventos `PieceCompleted { torrent_id, piece_index }`.
    fn subscribe_piece_events(&self) -> BoxStream<'static, PieceEvent>;

    /// Capacidades soportadas por este cliente.
    fn capabilities(&self) -> ClientCapabilities;
}

pub struct ClientCapabilities {
    pub supports_piece_priority: bool,
    pub supports_piece_events: bool,
    pub supports_real_time_progress: bool,
}
```

Cuando un cliente no soporta una operación (por ejemplo, prioridad por pieza), la operación correspondiente devuelve `AdapterError::Unsupported` y los componentes que la usan caen a un fallback (en el caso de prioridad por pieza, fallback a prioridad por archivo, asumiendo el archivo entero como unidad).

Esta abstracción permite añadir soporte para clientes nuevos en el futuro implementando el trait, sin tocar el resto del sistema. En el MVP solo se implementa para qBittorrent vía su Web API; otros clientes se irán añadiendo en fases posteriores.

---

## 18. Protocolo IPC

### 18.1 Transporte y codificación

Socket Unix, mensajes MessagePack precedidos por longitud u32 LE.

### 18.2 Mensajes

```rust
enum Request {
    Status,
    PrefsGet(Option<String>),
    PrefsSet(HashMap<String, String>),
    Mount(TorrentId),
    Unmount(TorrentId),
    Shutdown,
    Version,
}

enum Response {
    Ok,
    Status(StatusInfo),
    Prefs(HashMap<String, String>),
    Error(String),
    Version(String),
}
```

### 18.3 Validación de prefs

El servidor IPC valida que los valores enviados en `PrefsSet` cumplen las restricciones:

- `audio_langs`: lista de hasta 5 códigos ISO 639. Más de 5 → error.
- `sub_langs`: lista de hasta 5 códigos ISO 639. Más de 5 → error.
- `audio_codec`: lista de códecs reconocidos por el sistema.
- `res`: uno de los valores válidos.
- `read_mode`: `block`, `timeout`, o `nonblock`.
- `read_timeout_ms`: u32, > 0.

---

## 19. Roadmap de implementación

### Fase 0: setup

Workspace Cargo, crates esqueleto, CI básico.

### Fase 1: schema y serialización

Crates `videofuser-binstruct` (schema y serialización), `videofuser-vfr`. Tests de round-trip.

### Fase 2: parser MVP

Crate `videofuser-parser-common`. Bins `parser-aac`, `parser-ac3`, `parser-h264`. Tests con archivos reales pequeños.

### Fase 3: binstruct gen

Bin `binstruct` con subcomandos `gen`, `inspect`, `verify`, `verify-manifest`. Validaciones obligatorias (FlagDefault, OriginalLanguage, consistencia CBR, integridad). Tests end-to-end del lado publicador.

### Fase 4: muxer

Crate `videofuser-muxer` con modos raw/transform. Tests con binstructs sintéticos. Verificación de que los MKV virtuales filtrados se decodifican correctamente.

### Fase 5: filtro

Crate `videofuser-filter` con la lógica de resolución de audio y subs. Tests unitarios con muchos casos.

### Fase 6: FUSE

Crate `videofuser-fs` con sidecar visibility según filtro. Tests con mountpoints temporales.

### Fase 7: demonio MVP

Bin `videofuser`: integración de muxer + fuser + filter + IPC + watcher básico. Modo `block` por defecto. CLI completa. Sin track controller aún.

### Fase 8: integración con cliente torrent

Watcher con API del cliente. Track controller. Notificaciones de piezas.

### Fase 9: parsers adicionales

`parser-h265`, `parser-av1`, `parser-mp3`, `parser-eac3`, `parser-dts`, `parser-truehd`.

### Fase 10: testing extensivo

Tests con MKVs reales. Benchmarks. Tests con varios reproductores.

### Fase 11: polish y release

Documentación, empaquetado, release v1.0 del software.

---

## 20. Apéndices

### 20.1 Apéndice A: Tabla resumen de IDs EBML

Ver capítulo 7.3.

### 20.2 Apéndice B: Tabla sugerida de variants

| Valor | Descripción |
|---|---|
| 00 | Doblaje profesional principal / Subs estándar |
| 01 | Segundo doblaje profesional / Subs alternativos |
| 02 | Doblaje fan / Subs fan |
| 03 | Comentarios del director |
| 04 | Comentarios del reparto |
| 05 | Banda sonora aislada |
| 06 | Subs forzados |
| 07 | Subs SDH (sordos) |
| 08-99 | A discreción del publicador |

### 20.3 Apéndice C: Códigos de error

Esquema sugerido a definir durante implementación:

- 0: éxito.
- 1: error de uso.
- 2: error de I/O.
- 3: error de formato.
- 4: error de configuración.
- 5: error interno.
- 6: error de validación (e.g., MKV intermedio sin FlagDefault).

### 20.4 Apéndice D: Estados del demonio

- `Detected`: torrent detectado, binstruct/manifest aún no descargados.
- `Loading`: descargando binstruct/manifest, parseando.
- `Active`: completamente operativo.
- `Paused`: usuario suspendió.
- `Errored`: error irrecuperable.

### 20.5 Apéndice E: Decisiones arquitectónicas registradas

| Decisión | Motivación |
|---|---|
| Reconstrucción determinista (no bit-exact) | Reduce tamaño del binstruct. |
| Eliminación de lacing | Simplifica el muxer. Overhead despreciable. |
| Subtítulos como sidecars | Evita reescribir Tracks. Auto-detección. |
| Parsers como binarios independientes | Modularidad. Extensibilidad. |
| Bloqueo indefinido por defecto | Robustez. Evita corrupción visible. |
| Single-instance del demonio | Estado consistente. IPC simple. |
| Mountpoint multiplexor | Un solo punto de presencia. |
| EBML propio del binstruct | Formato extensible. |
| VFR como archivos separados | Descarga selectiva por pista. |
| Hashes SHA-256 por archivo | Verificación. |
| Bytes 8 fijos por record VFR | Acceso aleatorio O(1). |
| MKV intermedio sin subs | Evita inconsistencias. |
| Conversión Annex B → AVCC en muxer | Formato MKV correcto sin parsear códecs en receptor. |
| Política `block` por defecto | Mejor UX que devolver ceros corruptos. |
| Naming con prefijo `<base>` | Browsing humano + desambiguación. |
| **Filtrado por listas de hasta 5 idiomas** | **Reduce drásticamente número de pistas en MKV virtual.** |
| **Listas de audio y subs independientes** | **Flexibilidad: el usuario puede preferir audio italiano y subs en español sin contradicción.** |
| **Fallback "todas las pistas" si ningún preferido disponible** | **Evita MKVs vacíos en casos raros.** |
| **FlagDefault=1 obligatorio en MKV intermedio** | **Determina sin ambigüedad el idioma original.** |
| **MkvSkeleton trozeado en PreTracksBlob + TrackEntries[] + PostTracksBlob** | **Permite filtrado dinámico del Tracks element.** |
| **SeekHead siempre regenerado al final** | **Evita problemas de offsets desfasados.** |
| **Modos raw/transform del muxer** | **Codec-agnóstico para el receptor.** |
| **Sidecars ocultos hasta completar descarga** | **Evita reproducción de subs corruptos.** |
| **Extensión `.vfr.zst` para VFR comprimidos** | **Detección automática + claridad humana.** |

### 20.6 Apéndice F: Diagrama de flujo del filtro de audio

```
                    ┌──────────────────────┐
                    │  Prefs.audio_langs   │
                    │  (hasta 5 idiomas)   │
                    └──────────┬───────────┘
                               │
                               ▼
              ┌────────────────────────────────────┐
              │ ¿Pistas con LanguageCode in        │
              │  audio_langs?                      │
              └─────────────┬──────────────────────┘
                  Sí (al menos una)        No
                            │              │
                            ▼              ▼
            ┌───────────────────────┐ ┌──────────────────────────┐
            │ Incluir esas pistas + │ │ ¿Pista con LanguageCode  │
            │ original (si distinta)│ │  == OriginalLanguage?    │
            └─────────────┬─────────┘ └────────────┬─────────────┘
                          │                Sí      │      No
                          │                        │      │
                          │                        ▼      ▼
                          │            ┌────────────┐ ┌──────────────────┐
                          │            │ Solo el    │ │ FALLBACK:        │
                          │            │ original   │ │ Todas las pistas │
                          │            │            │ │ de audio         │
                          │            └─────┬──────┘ └──────┬───────────┘
                          │                  │               │
                          ▼                  ▼               ▼
                ┌──────────────────────────────────────────────────┐
                │     Conjunto final de TrackIds incluidos         │
                │     + Todas las pistas de vídeo (8)              │
                └──────────────────────────────────────────────────┘
```

### 20.7 Apéndice G: Ejemplo completo de prefs

```toml
# ~/.config/videofuser/prefs.toml

[langs]
audio = ["es", "en", "it", "fr", "de"]      # hasta 5
subs = ["es", "en", "fr", "it", "pt"]       # hasta 5, independiente

[codecs]
audio = ["truehd", "eac3", "dts", "ac3", "aac", "mp3"]

[video]
resolution = "1080p"

[reads]
mode = "block"
timeout_ms = 1000

[mountpoint]
path = "/mnt/videofuser"

[fuse]
direct_io = false
kernel_cache = true
```

### 20.8 Apéndice H: Guía de implementación — caveats conocidos

Este apéndice recopila zonas donde la especificación define el comportamiento correcto pero la implementación requiere atención particular. No son cambios al diseño; son notas para quien implemente.

#### H.1 Estado del muxer entre `read()` fragmentados por el kernel

El kernel suele emitir `read()` al FUSE en bloques de 4 KB a 128 KB, no necesariamente alineados con frames del MKV virtual. Un mismo SimpleBlock puede dividirse en varios `read()` consecutivos, e incluso un NAL unit dentro de un frame de vídeo puede caer parcialmente en el límite entre dos `read()`.

El muxer debe diseñarse como **stateless por `read()`**: cada llamada parte del offset solicitado y produce los bytes solicitados sin asumir continuidad con la llamada anterior. Esto significa:

- Para servir un rango cualquiera, el muxer recalcula desde cero qué frame o sección lo cubre.
- Si el rango cae en medio de un payload AVCC, el muxer aplica la conversión Annex B → AVCC del frame entero y devuelve el subrango solicitado.
- No se mantiene caché de frames convertidos entre llamadas (a menos que se introduzca como optimización explícita, ver H.2).

La capa 4 del muxer (read coordinator) es la responsable de manejar esta característica.

#### H.2 Caché de frames AVCC convertidos (optimización opcional)

Si los benchmarks muestran que la conversión Annex B → AVCC en cada `read()` es un cuello de botella, se puede añadir un caché LRU de frames AVCC ya convertidos. Cada entrada es `(track_id, frame_index) → bytes_avcc`. Tamaño limitado configurable (por defecto: ninguno, sin caché en MVP).

La invalidación del caché se hace cuando llegan piezas nuevas que afectan al frame, lo cual el `download_state` notifica.

#### H.3 mmap en reproductores y `inval_inode`

La invalidación de caché vía `Notifier::inval_inode` es un *hint* al kernel: el kernel descarta sus páginas cacheadas para el rango invalidado. Sin embargo, **algunos reproductores leen archivos vía `mmap()`**, lo que les da páginas mapeadas directamente desde el page cache. Si el reproductor mapea un rango y luego ese rango se invalida, el comportamiento depende de la implementación del kernel.

Soluciones:

- Configurar FUSE con `direct_io = true` (pref correspondiente). Esto desactiva el page cache para el archivo y cada `read()` y `mmap()` va al FUSE directamente. Penalización de rendimiento.
- Documentar a los usuarios que algunos reproductores en algunos sistemas pueden mostrar contenido obsoleto si hacen mmap. La solución es cerrar y reabrir el archivo.

#### H.4 Granularidad de `inval_inode`

Invalidar rangos pequeños (un SimpleBlock cada vez que llega una pieza) genera overhead alto en el kernel. El recomendado es **batchear invalidaciones**:

- Acumular cambios al `download_state` durante un intervalo (ej. 100 ms).
- Al final del intervalo, computar el rango virtual mínimo que cubre todos los cambios y hacer un único `inval_inode`.

Para casos extremos (descargas masivas en paralelo), invalidar el inode entero con `inval_inode(ino, 0, u64::MAX)` es válido y barato.

#### H.5 Detección de start codes 3-byte vs 4-byte en Annex B

Un bitstream Annex B puede tener start codes de 3 bytes (`0x00 0x00 0x01`) o 4 bytes (`0x00 0x00 0x00 0x01`). mkvextract por defecto usa 4 bytes, pero algunos otros extractores pueden emitir 3 bytes en algunas posiciones (típicamente entre NAL units del mismo frame).

El muxer debe detectar dinámicamente cuál es: si los bytes en la posición indicada por el VFR son `0x00 0x00 0x00 0x01`, es 4-byte; si son `0x00 0x00 0x01`, es 3-byte. La conversión a AVCC es la misma (descartar el start code, prepender length u32), independiente del tamaño del start code.

El parser debe almacenar en el VFR la posición y longitud del NAL **excluyendo el start code**, de modo que el muxer pueda saltar la cantidad correcta. Si los nal_lengths almacenan solo el payload del NAL (sin start code), basta con saltar `frame_offset` hasta encontrar el primer byte no-cero.

#### H.6 Cluster_offsets para Cues

La generación de Cues requiere conocer los offsets virtuales de inicio de cada cluster. Estos se calculan iterativamente al construir el layout (capa 1 del muxer):

```
offset = pre_tracks_blob_size + tracks_element_size + post_tracks_blob_size
for each cluster N:
    cluster_offsets[N] = offset
    offset += cluster_size(N)
```

Donde `cluster_size(N)` se calcula como `cluster_header_size + sum(simple_block_size(track, frame))` para todos los frames del cluster en el filtro.

Este array se computa al cargar el binstruct y aplicar el filtro, y se mantiene en memoria mientras el filtro sea activo. Cambios de filtro disparan recálculo.

#### H.7 Validación contra reproductores reales en MVP

Para el MVP se valida contra estos reproductores en orden de prioridad:

1. **mpv**: el más tolerante, buena referencia base.
2. **ffplay**: el más estricto en interpretación del bitstream, atrapa errores de muxing.
3. **VLC**: el más popular, tiene quirks pero está bien soportado.

Otros reproductores (Kodi, Plex, Jellyfin) se prueban en fases posteriores. No se garantiza compatibilidad inicial con reproductores de hardware (TVs, decodificadores externos), aunque en teoría debería funcionar al producir un MKV estándar.

#### H.8 Validación de timing con contenido VFR

El contenido con frame rate variable (VFR) o discontinuidades temporales ha sido históricamente fuente de bugs en muxers Matroska. Tipos de contenido a probar específicamente:

- Anime con frame rate variable según la escena.
- Películas con cambio de frame rate entre intro y contenido principal.
- Contenido con B-frames complejos y reordenamiento de timestamps.
- Bluray rips con marcadores de capítulo en timestamps no alineados con frames.

Tests con estos tipos de contenido deben formar parte de la suite de integración antes de declarar el MVP completo.

---

## 21. Cierre

Esta especificación se considera **cerrada y completa** para la fase MVP. Cambios futuros se reflejarán en versiones posteriores del schema y revisiones del documento.

La implementación procede según el roadmap del capítulo 19, partiendo del setup del workspace Cargo.

---

**Fin del documento**