#!/bin/sh
# Starts the Rewind desktop app from its tarball on any Linux distribution.
# The app uses the system's libxkbcommon, libxcb and graphics drivers, and
# brings its own fonts. When the rewind command is not on PATH but its
# tarball is unpacked next to this one, the app uses that for forks and
# for reading files at a step.
here=$(dirname "$(readlink -f "$0")")
top="$here/.."

: "${REWIND_APP_FONTS:=$top/share/rewind-app/fonts}"
export REWIND_APP_FONTS

if [ -z "${REWIND_BIN:-}" ] && ! command -v rewind >/dev/null 2>&1; then
  for candidate in "$top"/../rewind-*-x86_64-linux/bin/rewind; do
    if [ -x "$candidate" ]; then
      REWIND_BIN=$candidate
      export REWIND_BIN
      break
    fi
  done
fi

# Name the system libraries that are missing, with the packages that
# provide them, rather than leave it to the loader's error.
ldconfig=$(command -v ldconfig || echo /sbin/ldconfig)
if [ -x "$ldconfig" ]; then
  libs=$("$ldconfig" -p 2>/dev/null)
  missing=""
  for lib in libxkbcommon.so.0 libxkbcommon-x11.so.0 libxcb.so.1; do
    case "$libs" in
      *"$lib "*) ;;
      *) missing="$missing $lib" ;;
    esac
  done
  if [ -n "$missing" ]; then
    echo "rewind-app: these system libraries are missing:$missing" >&2
    echo "  Debian and Ubuntu: sudo apt install libxkbcommon-x11-0 libvulkan1 mesa-vulkan-drivers" >&2
    echo "  Fedora: sudo dnf install libxkbcommon-x11 vulkan-loader mesa-vulkan-drivers" >&2
    exit 1
  fi
  case "$libs" in
    *"libvulkan.so.1 "*) ;;
    *) echo "rewind-app: no Vulkan loader (libvulkan.so.1); install your distribution's vulkan loader and Mesa Vulkan drivers if the window stays empty" >&2 ;;
  esac
fi

exec "$top/libexec/rewind-app" "$@"
