# Stage libgphoto2 headers/.pc + libclang for bindgen, without touching the system.
# Runtime libgphoto2.so.6 and the camlibs/iolibs are already present system-wide (gvfs pulls
# them in); only the -dev half and libclang had to be staged.
STAGE=/tmp/claude-1000/-home-artiom-repos-diirc-astrocli/bc69c063-787a-4b51-89c6-826ace821aa9/scratchpad/gp-stage/prefix

# pkg-config finds the staged .pc files; SYSROOT_DIR rewrites their hardcoded prefix=/usr
# into the extracted tree so -I/-L land on the staged headers and .so symlinks.
export PKG_CONFIG_PATH="$STAGE/usr/lib/x86_64-linux-gnu/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR="$STAGE"

# bindgen needs libclang itself plus clang's builtin headers (stddef.h and friends).
export LIBCLANG_PATH="$STAGE/usr/lib/llvm-20/lib"
export BINDGEN_EXTRA_CLANG_ARGS="-I$STAGE/usr/lib/llvm-20/lib/clang/20/include"

# The .so symlinks in the staged prefix point at versioned libs that only exist under /usr,
# so the linker needs both search paths; at run time the system copy is what loads.
export LD_LIBRARY_PATH="/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
