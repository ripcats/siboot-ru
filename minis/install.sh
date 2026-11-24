#!/bin/sh
set -eu

BASE="${SIBOOT_BASE:-https://raw.githubusercontent.com/ripcats/siboot-ru/main/minis}"
SHA256="b09d2514003f61b518626af51e5efaeee471defc1a453b4a33984260bf120f82"
SIZE=12143448

DIR="$HOME/.siboot/bin"
BIN="$DIR/siboot"

FANCY=
if [ -t 1 ] && [ "${TERM:-}" != dumb ] && [ -z "${NO_COLOR:-}" ]; then
  FANCY=1
fi

COLS=$({ stty size < /dev/tty; } 2> /dev/null | cut -d ' ' -f 2) || COLS=
case "$COLS" in
  '' | *[!0-9]*) COLS=40 ;;
esac

say() {
  if [ -z "$FANCY" ]; then
    echo "$3"
    return
  fi
  printf '\r\033[2K'
  first=1
  printf '%s\n' "$3" | fold -s -w $((COLS - 5)) | while IFS= read -r line; do
    if [ -n "$first" ]; then
      printf '  \033[%sm%s\033[0m %s\n' "$2" "$1" "$line"
      first=
    else
      printf '    %s\n' "$line"
    fi
  done
}

ok() { say ✓ '1;32' "$1"; }

fail() {
  say ✗ '1;31' "$1" >&2
  exit 1
}

case "$(uname -m)" in
  aarch64 | arm64) ;;
  *) fail "siboot работает в Open Minis на iPhone (aarch64); текущая архитектура — $(uname -m)." ;;
esac

fetch() {
  if command -v curl > /dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  else
    wget -qO "$2" "$1"
  fi
}

sha() { sha256sum "$1" | cut -d ' ' -f 1; }

repeat() {
  n=$1
  out=
  while [ "$n" -gt 0 ]; do
    out="$out$2"
    n=$((n - 1))
  done
  printf '%s' "$out"
}

megabytes() {
  tenths=$(($1 * 10 / 1048576))
  printf '%d.%d' $((tenths / 10)) $((tenths % 10))
}

frame() {
  have=$1
  case $2 in
    0) spin=⠋ ;; 1) spin=⠙ ;; 2) spin=⠹ ;; 3) spin=⠸ ;; 4) spin=⠼ ;;
    5) spin=⠴ ;; 6) spin=⠦ ;; 7) spin=⠧ ;; 8) spin=⠇ ;; *) spin=⠏ ;;
  esac
  percent=$((have * 100 / SIZE))
  [ "$percent" -gt 99 ] && percent=99
  size=" $(megabytes "$have")/$(megabytes "$SIZE") МБ"
  cells=$((COLS - 29))
  if [ "$cells" -ge $((12 + ${#size})) ]; then
    cells=$((cells - ${#size}))
  else
    size=
  fi
  [ "$cells" -gt 24 ] && cells=24
  [ "$cells" -lt 4 ] && cells=4
  full=$((percent * cells / 100))
  printf '\r\033[2K  \033[36m%s\033[0m Загрузка siboot \033[36m%s\033[0m\033[2m%s\033[0m %3d%%%s' \
    "$spin" "$(repeat "$full" ━)" "$(repeat $((cells - full)) ─)" "$percent" "$size"
}

download() {
  rm -f "$BIN.part"
  if [ -z "$FANCY" ]; then
    echo "Загрузка siboot ($(megabytes "$SIZE") МБ)…"
    fetch "$BASE/bin/siboot-aarch64" "$BIN.part"
    return
  fi
  fetch "$BASE/bin/siboot-aarch64" "$BIN.part" < /dev/null 2> /dev/null &
  pid=$!
  i=0
  while kill -0 "$pid" 2> /dev/null; do
    have=$({ wc -c < "$BIN.part"; } 2> /dev/null || echo 0)
    frame $((have + 0)) "$i"
    i=$(((i + 1) % 10))
    sleep 0.1 2> /dev/null || sleep 1
  done
  wait "$pid"
}

mkdir -p "$DIR"
if [ -f "$BIN" ] && [ "$(sha "$BIN")" = "$SHA256" ]; then
  ok "siboot уже обновлён"
else
  if ! download; then
    rm -f "$BIN.part"
    fail "Не удалось загрузить siboot. Проверьте подключение к интернету и повторите попытку."
  fi
  if [ "$(sha "$BIN.part")" != "$SHA256" ]; then
    rm -f "$BIN.part"
    fail "Контрольная сумма siboot не совпала. Повторите попытку через несколько минут."
  fi
  chmod 755 "$BIN.part"
  mv "$BIN.part" "$BIN"
  ok "siboot загружен и проверен ($(megabytes "$SIZE") МБ)"
fi

ln -sf "$BIN" /usr/local/bin/siboot 2> /dev/null || true

if [ -t 0 ]; then
  exec "$BIN" "$@"
elif [ -r /dev/tty ] && (: < /dev/tty) 2> /dev/null; then
  exec "$BIN" "$@" < /dev/tty
else
  exec "$BIN" "$@"
fi
