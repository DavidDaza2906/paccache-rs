# paccache-rs — plan

Reemplazo directo en Rust de `paccache` (pacman-contrib 1.13.1, `/usr/bin/paccache`,
431 líneas de bash + awk que llama a `pacsort`, `stat`, `xargs`, `rm`, `mv`).

## Objetivo

1. **Compatibilidad total**: mismas opciones, misma salida (stdout, stderr, código de
   salida) y mismo efecto en disco que `paccache` 1.13.1, en todos los casos que cubra
   la batería diferencial. Diferencias permitidas: las 4 divergencias acordadas (ver
   "Estado final").
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
| `-k` | entero no negativo (`10#` → ceros a la izquierda permitidos), si no: `argument to option -k must be a non-negative integer`. (El original además evalúa el valor como aritmética de bash por `declare -i`: no se replica, divergencia 1.) |
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
tests/compat.rs + tests/support/      diferencial vs /usr/bin/paccache → agente D
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
3. **Integración** (hecho): fusiona ramas, escribe `main.rs`, corre la batería D,
   corrige diferencias hasta 0 fallos, benchmark sobre la caché real en `-d`
   (solo lectura), README en inglés, PKGBUILD, CI de GitHub Actions.
4. **Revisión del usuario** (pendiente) antes de crear el repo público y publicar en
   crates.io/AUR.

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

## Estado final (v0.1, integración terminada)

Ramas `agent-a..d` fusionadas en `main` y borradas, junto con sus worktrees.
`src/main.rs` sigue el flujo del script. `cargo clippy --all-targets -- -D warnings`
está limpio y `cargo test --release` (también con `--include-ignored`) en verde.

| Batería | Resultado |
|---|---|
| `tests/compat.rs` diferencial (exit, stdout, stderr, árbol final) | 151 escenarios, 0 diferencias |
| `divergence_*` (afirman nuestro comportamiento) | 22 tests en verde |
| `vercmp` vs `/usr/bin/vercmp` | 109 409 pares, 0 diferencias |
| `pacsort --files` | 4 000 listas / 152 163 rutas aleatorias + caché real (10 518, 3 órdenes), 0 diferencias |
| Caché real, `-d` (13 combinaciones × locales C y en_US.UTF-8, + tty con color) | idéntico byte a byte |

Benchmark (bucle de respaldo, sin hyperfine), caché real en `-d`: 128.6 → 22.3 ms
(`-d`), 208.9 → 27.3 ms (`-d -k 1`), 136.0 → 32.9 ms (`--min-atime`). Tablas
completas en el README.

### Divergencias acordadas (con tests `divergence_*`)

1. `-k`: solo `[0-9]+` en base 10 hasta i64::MAX (el original usa aritmética de bash).
2. `%` en el directorio de `-m`: se imprime literal (el original lo usa como formato).
3. Segfault de `pacsort` con un basename sin `-` en una ruta con `-`: no se replica, se
   ordena normalmente (`pacsort_crashes` queda solo como diagnóstico).
4. `-V/--version`: texto propio.

### Correcciones hechas en la integración

- `scan` y `output::summarize` ordenaban con `slice::sort_by` y ahora usan el merge
  sort de glibc (`pkgname::pacsort_files_sort` / `glibc_sort_by`).
- El `die` de la comprobación de operaciones ahora respeta `--nocolor`.
- `pacman-conf CacheDir` se ejecuta en el mismo punto que en el script (antes de la
  comprobación de operaciones, vía `Env::default_cachedirs_needed`), así que su stderr
  sale antes del error "no operation specified", y si falta se imprime el
  `…: line 348: pacman-conf: command not found` de bash.
- Los errores de `find` (`--min-*time` con subdirectorios ilegibles) usan las comillas
  de gnulib según el charset de `LC_CTYPE` (`'…'` en C, `‘…’` en UTF-8). Se detectó
  comparando con la caché real; la batería tiene escenarios nuevos para este caso.
- Con 0 coincidencias del glob, el script pasa una línea vacía a `pacsort`/awk
  (`printf '%s\n'` sin argumentos), que no produce candidatos; los escenarios
  `empty_*` / `nopkgs_*` confirman que el resultado es idéntico.

### No replicado / pendiente

- Nombres de archivo con salto de línea; nombres con SUBSEP (`\x1c`) de awk; el caso
  `min_atime == -min_mtime != 0` del awk (ver docs de `select`).
- Traducciones (`gettext`): solo inglés.
- Publicación: crear el repo, rellenar `OWNER` en README/PKGBUILD/Cargo.toml y los
  `sha256sums`, publicar en crates.io y en el AUR (requiere revisión del usuario).
