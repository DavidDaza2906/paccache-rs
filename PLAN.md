# paccache-rs — plan

Reemplazo directo en Rust de `paccache` (pacman-contrib 1.13.1, `/usr/bin/paccache`,
431 líneas de bash + awk que llama a `pacsort`, `stat`, `xargs`, `rm`, `mv`).

## Objetivo

1. **Compatibilidad total**: mismas opciones, misma salida (stdout, stderr, código de
   salida) y mismo efecto en disco que `paccache` 1.13.1, en todos los casos que cubra
   la batería diferencial. Única diferencia permitida: el texto de `-V/--version`.
2. **Más rápido**: sin procesos hijos para listar, ordenar, filtrar ni sumar tamaños.
   Medido con `hyperfine` sobre la caché real (10 518 archivos, 35 GB) en `-d`.
3. **Publicable**: repo público, licencia GPL-2.0-or-later (como el original, del que se
   portan lógica y mensajes), README en inglés, PKGBUILD para el AUR.

## Fuera de alcance (v0.1)

- Traducciones (`gettext`): solo inglés, como en un sistema con `LANG=C`.
- `checkupdates`, `checkrebuild`, `pacdiff`: siguientes fases del mismo repo.

## Comportamiento que hay que replicar (leído del script)

| Tema | Detalle del original |
|---|---|
| Opciones | `parseopts` de libmakepkg: cortas agrupables (`-dvk2`), largas con `=` o separadas, **prefijos de largas no ambiguos** (`--rem` = `--remove`), error "option '--x' is ambiguous", `--` termina opciones; resto = whitelist. `-s` está en OPT_SHORT pero no hace nada. |
| Operación | exactamente una de `-d`, `-m DIR`, `-r`; si no: `no operation specified (use -h for help)`; si varias: `only one operation may be used at a time`. |
| `-k` | entero no negativo (`10#` → ceros a la izquierda permitidos), si no: `argument to option -k must be a non-negative integer`. |
| `-i` | lista separada por comas, o `-` = leer stdin (solo si stdin no es tty), acumulable. |
| `-u` | `pacman -Qq` → todos los instalados van a la blacklist; lista vacía → `failed to retrieve the list of installed packages`. |
| `--min-atime/--min-mtime` | `date -d "<texto>" +%s` (formato GNU "Date input formats"); con ellas el listado es **recursivo** (`find`) e incluye ocultos; sin ellas es glob **no recursivo** `*.pkg.tar!(*.sig)` (sin ocultos). |
| Cachedir | `-c` repetible; por defecto `pacman-conf CacheDir`. No existe → `cachedir '%s' does not exist or is not a directory`. |
| `-m DIR` | no existe → `destination directory '%s' does not exist or is not a directory`; relativo → se hace absoluto. |
| Root | si `-m`/`-r` y el cachedir o movedir no es escribible → `sudo rm/mv` (si no hay sudo: dos errores y salir). |
| Nombre de paquete | awk: `split(basename, "-")`; nombre = partes 1..n-3 unidas por `-`; arch = última parte hasta el primer `.`. Agrupación por (nombre, arch). |
| Orden | `pacsort --files` (nombre, luego `alpm_pkg_vercmp`); se marcan como candidatos todos menos los `keep` más nuevos de cada grupo; con min-atime/mtime solo si `atime < min` y/o `mtime < min` (atime/mtime con fracción, `%A@`). |
| `-a ARCH` | solo grupos con esa arch. |
| Firmas | por cada candidato, si existe `X.sig` también se borra/mueve y cuenta en el tamaño, pero no en el número de paquetes. |
| Salida | `msg` = `==> texto` a **stdout**; `error` = `==> ERROR: texto` a **stderr**; color solo si stderr es tty y sin `--nocolor`. `size_to_human`: base 1024, `%.2f` sin ceros finales, `B KiB MiB GiB TiB PiB EiB`, divide mientras `> 1024`. Línea vacía antes del resumen. `-q` silencia `msg` y el resumen. |
| Resumen | `finished: N packages removed` / `finished: N packages moved to 'DIR'` / `finished dry run: N candidates`, + ` (disk space saved: X)`. Sin candidatos: `no candidate packages found for pruning`, salida 0. |
| `-v` en `-d` | `Candidate packages:` y lista ordenada con pacsort: `-v` basename, `-vv` ruta completa, `-vvv` agrupado `nombre (arch):` + `  archivo`. `-z` = separador NUL para -v/-vv. En `-r/-m`, `-v` pasa `-v` a rm/mv. |
| `-f` | pasa `-f` a rm/mv. |

## Arquitectura (crate único, binario `paccache-rs`)

```
src/
  main.rs      orquestación (flujo idéntico al script)       → integración
  vercmp.rs    alpm_pkg_vercmp en Rust puro                  → agente A
  pkgname.rs   parseo de nombre de archivo + orden pacsort   → agente A
  cli.rs       parseopts compatible + fechas                 → agente B
  output.rs    msg/error/colores/size_to_human/resumen       → agente B
  scan.rs      listado de cachedirs (glob / recursivo)       → agente C
  select.rs    agrupación y selección de candidatos          → agente C
  actions.rs   firmas, tamaño, rm/mv (directo o vía sudo)    → agente C
tests/compat.rs + tests/fixtures.rs   diferencial vs /usr/bin/paccache → agente D
bench/        generador de caché falsa + hyperfine           → agente D
```

Las firmas públicas de cada módulo están fijadas en el andamiaje (`todo!()`), para que
los agentes trabajen en paralelo sin pisarse. Cambiar una firma pública requiere
avisarlo en el informe final.

Decisión sobre `vercmp`: implementación propia en Rust puro (sin enlazar libalpm, que
rompe al actualizar pacman), validada diferencialmente contra `/usr/bin/vercmp` con
miles de pares aleatorios + casos borde (epoch, pkgrel, alfanuméricos, `~`, vacíos).
`alpm-types` (crate oficial del proyecto ALPM) se evalúa como alternativa si es ligero.

## Fases

1. **Andamiaje** (hecho por el orquestador): Cargo.toml, módulos con firmas, este plan.
2. **Paralelo, cada agente en su worktree/rama**:
   - A `vercmp` + `pkgname` (+ orden `pacsort --files`), pruebas diferenciales.
   - B `cli` + `output`, pruebas unitarias de cada mensaje y opción.
   - C `scan` + `select` + `actions`, pruebas con directorios temporales.
   - D batería diferencial (`tests/compat.rs`) que ejecuta original y nuestro binario
     sobre cachés falsas generadas y compara stdout, stderr, código y estado del disco;
     generador de caché grande y script de benchmark.
3. **Integración** (un agente): fusiona ramas, escribe `main.rs`, corre la batería D,
   corrige diferencias hasta 0 fallos, benchmark sobre la caché real en `-d`
   (solo lectura), README en inglés, PKGBUILD, CI de GitHub Actions.
4. **Revisión del usuario** antes de crear el repo público y publicar en crates.io/AUR.

## Reglas para todos los agentes

- Nunca ejecutar `-r` ni `-m` sobre `/var/cache/pacman`; con la caché real solo `-d`.
- Nada de sudo.
- `cargo build`, `cargo clippy --all-targets -- -D warnings` y `cargo test` en verde
  antes de terminar.
- Código y comentarios en inglés; commits terminados en
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Criterio de terminado (v0.1)

- Batería diferencial con ≥ 60 escenarios, 0 diferencias (salvo `--version`).
- `vercmp` coincide con `/usr/bin/vercmp` en ≥ 100 000 pares.
- Benchmark documentado en el README.
