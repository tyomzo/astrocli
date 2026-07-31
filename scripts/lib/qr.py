#!/usr/bin/env python3
"""A QR encoder small enough to read, for scripts/demo-m1.sh.

Usage:  python3 scripts/lib/qr.py "https://example.invalid/"
        python3 scripts/lib/qr.py --self-test

# Why this exists rather than a dependency

The demo prints a URL and a token for a phone to scan, outdoors, in the dark. `qrencode` is the
obvious answer and is not installed on most machines; `pip install qrcode` puts a package manager
between an operator and a demo. M1-T16 asks for a pure shell-or-Python QR and no heavyweight
dependency, so this is the standard library and about two hundred lines.

# Scope, stated rather than discovered

Byte mode, error-correction level **L**, versions **1–5** — single Reed-Solomon block, which is
what keeps the encoder short: from version 6 upward the codewords are split into interleaved
blocks and the placement gains a whole layer. Version 5-L holds 106 bytes, and the longest thing
this script is asked to print is a URL and a 32-character token. Anything longer is **refused with
a message**, never truncated and never silently mis-encoded: a QR that scans to the wrong string is
worse than no QR, because the operator finds out by typing it in by hand anyway.

Level L rather than a higher one for the same reason: the code is displayed on a screen a foot from
the camera, not printed on a box that gets scuffed, so the redundancy would only cost version.

# The self-test

`--self-test` re-reads the finished matrix — unmasks it, walks the placement backwards — and
checks the codewords come back. That covers the two things most likely to be subtly wrong and
least likely to be noticed, since a QR reader either works or does not tell you why: the zigzag
module placement and the mask XOR. The Reed-Solomon half is checked against the generator
polynomial the spec tabulates.
"""

from __future__ import annotations

import sys

# --------------------------------------------------------------------------------------------
# GF(256), the field Reed-Solomon works in. Primitive polynomial 0x11D, as QR specifies.
# --------------------------------------------------------------------------------------------
EXP = [0] * 512
LOG = [0] * 256
_x = 1
for _i in range(255):
    EXP[_i] = _x
    LOG[_x] = _i
    _x <<= 1
    if _x & 0x100:
        _x ^= 0x11D
for _i in range(255, 512):
    EXP[_i] = EXP[_i - 255]


def _mul(a: int, b: int) -> int:
    if a == 0 or b == 0:
        return 0
    return EXP[LOG[a] + LOG[b]]


def _generator(degree: int) -> list[int]:
    """The generator polynomial for `degree` error-correction codewords."""
    poly = [1]
    for i in range(degree):
        # Multiply by (x - alpha^i); in GF(2) subtraction is XOR. Coefficients run
        # highest-degree first, so multiplying by x shifts a coefficient *down* an index and the
        # constant term picks up the alpha factor. Getting these two the wrong way round builds
        # the polynomial in reverse — which still produces plausible-looking codewords and a QR
        # that no reader will accept, and is what the self-test's tabulated vector caught.
        nxt = [0] * (len(poly) + 1)
        for j, coefficient in enumerate(poly):
            nxt[j] ^= coefficient
            nxt[j + 1] ^= _mul(coefficient, EXP[i])
        poly = nxt
    return poly


def _ec_codewords(data: list[int], count: int) -> list[int]:
    generator = _generator(count)
    remainder = list(data) + [0] * count
    for i in range(len(data)):
        factor = remainder[i]
        if factor == 0:
            continue
        for j, coefficient in enumerate(generator):
            remainder[i + j] ^= _mul(coefficient, factor)
    return remainder[len(data):]


# --------------------------------------------------------------------------------------------
# Version table, level L only: (total codewords, error-correction codewords).
# Data codewords are the difference. Versions 1-5 are one RS block each, which is the whole
# reason the range stops at 5 — see the module docstring.
# --------------------------------------------------------------------------------------------
VERSIONS = {1: (26, 7), 2: (44, 10), 3: (70, 15), 4: (100, 20), 5: (134, 26)}
# Alignment-pattern centre coordinates per version. Version 1 has none.
ALIGNMENT = {1: [], 2: [6, 18], 3: [6, 22], 4: [6, 26], 5: [6, 30]}
# Level L is `01` in the two format bits.
FORMAT_EC_L = 0b01


class TooLong(ValueError):
    """The payload does not fit in version 5-L."""


def _choose_version(length: int) -> int:
    for version in sorted(VERSIONS):
        total, ec = VERSIONS[version]
        # 4 bits of mode + 8 bits of length + the payload, in whole codewords.
        capacity = total - ec
        if length + 2 <= capacity:
            return version
    raise TooLong(
        f"{length} bytes does not fit in version 5-L (106 bytes). This encoder covers "
        f"versions 1-5 only; see scripts/lib/qr.py for why."
    )


def _codewords(data: bytes, version: int) -> list[int]:
    total, ec = VERSIONS[version]
    capacity = total - ec

    bits: list[int] = []

    def put(value: int, width: int) -> None:
        for shift in range(width - 1, -1, -1):
            bits.append((value >> shift) & 1)

    put(0b0100, 4)  # byte mode
    put(len(data), 8)  # character count, 8 bits for versions 1-9 in byte mode
    for byte in data:
        put(byte, 8)
    # Terminator: up to four zero bits, but never past the capacity.
    for _ in range(min(4, capacity * 8 - len(bits))):
        bits.append(0)
    while len(bits) % 8:
        bits.append(0)

    words = [int("".join(str(bit) for bit in bits[i:i + 8]), 2) for i in range(0, len(bits), 8)]
    # Pad alternately with the two bytes the spec names, until the data capacity is full.
    pad = [0xEC, 0x11]
    while len(words) < capacity:
        words.append(pad[(len(words) - len(bits) // 8) % 2])
    return words + _ec_codewords(words, ec)


# --------------------------------------------------------------------------------------------
# The matrix.
# --------------------------------------------------------------------------------------------
def _reserved(size: int, version: int) -> list[list[bool]]:
    """Which modules are function patterns, and therefore not available to data."""
    taken = [[False] * size for _ in range(size)]

    def block(row: int, col: int, height: int, width: int) -> None:
        for r in range(row, row + height):
            for c in range(col, col + width):
                if 0 <= r < size and 0 <= c < size:
                    taken[r][c] = True

    # Finders plus their separators and the format-information strips beside them.
    block(0, 0, 9, 9)
    block(0, size - 8, 9, 8)
    block(size - 8, 0, 8, 9)
    # Timing patterns.
    block(6, 0, 1, size)
    block(0, 6, size, 1)
    # Alignment patterns, minus the three that would sit on a finder.
    centres = ALIGNMENT[version]
    for r in centres:
        for c in centres:
            if (r < 9 and c < 9) or (r < 9 and c > size - 10) or (r > size - 10 and c < 9):
                continue
            block(r - 2, c - 2, 5, 5)
    return taken


def _place_function_patterns(matrix: list[list[bool]], size: int, version: int) -> None:
    def finder(row: int, col: int) -> None:
        for r in range(-1, 8):
            for c in range(-1, 8):
                if not (0 <= row + r < size and 0 <= col + c < size):
                    continue
                inside = 0 <= r < 7 and 0 <= c < 7
                dark = inside and (
                    r in (0, 6) or c in (0, 6) or (2 <= r <= 4 and 2 <= c <= 4)
                )
                matrix[row + r][col + c] = dark

    finder(0, 0)
    finder(0, size - 7)
    finder(size - 7, 0)

    for i in range(size):
        matrix[6][i] = i % 2 == 0
        matrix[i][6] = i % 2 == 0

    centres = ALIGNMENT[version]
    for r in centres:
        for c in centres:
            if (r < 9 and c < 9) or (r < 9 and c > size - 10) or (r > size - 10 and c < 9):
                continue
            for dr in range(-2, 3):
                for dc in range(-2, 3):
                    matrix[r + dr][c + dc] = max(abs(dr), abs(dc)) != 1

    # The one module that is always dark (spec: (4 * version + 9, 8)).
    matrix[4 * version + 9][8] = True


def _mask(pattern: int, row: int, col: int) -> bool:
    if pattern == 0:
        return (row + col) % 2 == 0
    if pattern == 1:
        return row % 2 == 0
    if pattern == 2:
        return col % 3 == 0
    if pattern == 3:
        return (row + col) % 3 == 0
    if pattern == 4:
        return (row // 2 + col // 3) % 2 == 0
    if pattern == 5:
        return (row * col) % 2 + (row * col) % 3 == 0
    if pattern == 6:
        return ((row * col) % 2 + (row * col) % 3) % 2 == 0
    return ((row + col) % 2 + (row * col) % 3) % 2 == 0


def _data_positions(size: int, taken: list[list[bool]]) -> list[tuple[int, int]]:
    """Module coordinates in placement order: two-wide columns, right to left, zigzagging."""
    positions = []
    upward = True
    col = size - 1
    while col > 0:
        if col == 6:  # the vertical timing pattern is not a data column
            col -= 1
        rows = range(size - 1, -1, -1) if upward else range(size)
        for row in rows:
            for c in (col, col - 1):
                if not taken[row][c]:
                    positions.append((row, c))
        upward = not upward
        col -= 2
    return positions


def _format_bits(mask: int) -> int:
    """The 15-bit format information: five bits of level-and-mask, BCH(15,5), then the mask XOR.

    The final XOR with 0b101010000010010 is not decoration — it is what stops an all-zero format
    (level M, mask 0) from being fifteen light modules, which a reader cannot distinguish from
    quiet zone.
    """
    value = (FORMAT_EC_L << 3) | mask
    remainder = value << 10
    generator = 0b101_0011_0111
    for shift in range(4, -1, -1):
        if remainder & (1 << (shift + 10)):
            remainder ^= generator << shift
    return ((value << 10) | remainder) ^ 0b101_0100_0001_0010


def _place_format(matrix: list[list[bool]], size: int, mask: int) -> None:
    bits = _format_bits(mask)
    for i in range(15):
        bit = (bits >> i) & 1 == 1
        # The copy beside the top-left finder.
        if i < 6:
            matrix[8][i] = bit
        elif i == 6:
            matrix[8][7] = bit
        elif i == 7:
            matrix[8][8] = bit
        elif i == 8:
            matrix[7][8] = bit
        else:
            matrix[14 - i][8] = bit
        # The split copy beside the other two.
        if i < 8:
            matrix[8][size - 1 - i] = bit
        else:
            matrix[size - 15 + i][8] = bit


def _penalty(matrix: list[list[bool]], size: int) -> int:
    """The spec's four penalty rules, used to pick a mask."""
    score = 0
    # Rule 1: runs of five or more.
    for line in list(matrix) + [list(col) for col in zip(*matrix)]:
        run, previous = 1, line[0]
        for module in line[1:]:
            if module == previous:
                run += 1
            else:
                if run >= 5:
                    score += run - 2
                run, previous = 1, module
        if run >= 5:
            score += run - 2
    # Rule 2: 2x2 blocks of one colour.
    for r in range(size - 1):
        for c in range(size - 1):
            quad = (matrix[r][c], matrix[r][c + 1], matrix[r + 1][c], matrix[r + 1][c + 1])
            if all(quad) or not any(quad):
                score += 3
    # Rule 3: the finder-like 1:1:3:1:1 pattern.
    finder = [True, False, True, True, True, False, True, False, False, False, False]
    for line in list(matrix) + [list(col) for col in zip(*matrix)]:
        for i in range(size - 10):
            window = line[i:i + 11]
            if window == finder or window == finder[::-1]:
                score += 40
    # Rule 4: deviation from an even split of dark and light.
    dark = sum(sum(1 for module in row if module) for row in matrix)
    percent = dark * 100 // (size * size)
    score += 10 * min(abs(percent - 50) // 5, abs(percent - 50) // 5)
    return score


def encode(text: str) -> list[list[bool]]:
    """Encode `text` and return the finished module matrix.

    Raises `TooLong` when the payload exceeds version 5-L.
    """
    data = text.encode("utf-8")
    version = _choose_version(len(data))
    size = 17 + 4 * version
    words = _codewords(data, version)
    bits = [(word >> shift) & 1 for word in words for shift in range(7, -1, -1)]

    taken = _reserved(size, version)
    positions = _data_positions(size, taken)
    # Remainder bits beyond the codewords stay light; there are never more than seven.
    bits += [0] * (len(positions) - len(bits))

    best = None
    for mask in range(8):
        matrix = [[False] * size for _ in range(size)]
        _place_function_patterns(matrix, size, version)
        for (row, col), bit in zip(positions, bits):
            matrix[row][col] = (bit == 1) != _mask(mask, row, col)
        _place_format(matrix, size, mask)
        score = _penalty(matrix, size)
        if best is None or score < best[0]:
            best = (score, matrix, mask)
    assert best is not None
    return best[1]


def render(matrix: list[list[bool]], quiet: int = 4) -> str:
    """Render as half-block characters with explicit colours.

    Explicit black and white rather than the terminal's own foreground and background: a QR reader
    needs dark modules dark, and a script that assumed a dark terminal would produce an inverted,
    unscannable code on a light one. Half blocks put two module rows in one character cell, which
    keeps a version-3 code inside a normal terminal without the modules going oblong.
    """
    size = len(matrix)
    padded = [[False] * (size + 2 * quiet) for _ in range(quiet)]
    for row in matrix:
        padded.append([False] * quiet + list(row) + [False] * quiet)
    padded += [[False] * (size + 2 * quiet) for _ in range(quiet)]
    if len(padded) % 2:
        padded.append([False] * len(padded[0]))

    lines = []
    for r in range(0, len(padded), 2):
        line = []
        for c in range(len(padded[0])):
            # A dark module must render dark; `True` is dark, so the colours invert.
            upper = padded[r][c]
            lower = padded[r + 1][c]
            fg = "\033[38;5;16m" if upper else "\033[38;5;231m"
            bg = "\033[48;5;16m" if lower else "\033[48;5;231m"
            line.append(f"{fg}{bg}▀")
        lines.append("".join(line) + "\033[0m")
    return "\n".join(lines)


# --------------------------------------------------------------------------------------------
# The self-test: read the matrix back and check it says what was put in.
# --------------------------------------------------------------------------------------------
def _self_test() -> int:
    failures = 0

    # Two independent vectors from the specification, because the round-trip below only proves
    # this file agrees with itself — it would happily pass on a code no reader could scan.
    #
    # The generator polynomial for 7 EC codewords (alpha exponents 0, 87, 229, 146, 149, 238,
    # 102, 21).
    expected = [1, 127, 122, 154, 164, 11, 68, 117]
    if _generator(7) != expected:
        print(f"FAIL: generator(7) = {_generator(7)}, expected {expected}")
        failures += 1

    # The tabulated 15-bit format strings for level L, masks 0-7.
    tabulated = [
        "111011111000100",
        "111001011110011",
        "111110110101010",
        "111100010011101",
        "110011000101111",
        "110001100011000",
        "110110001000001",
        "110100101110110",
    ]
    for mask, expected_bits in enumerate(tabulated):
        produced = format(_format_bits(mask), "015b")
        if produced != expected_bits:
            print(f"FAIL: format bits for L/mask {mask} = {produced}, expected {expected_bits}")
            failures += 1

    for text in [
        "http://192.168.1.20:18470/",
        "https://astro.example.invalid/field",
        "x",
        "A" * 106,
    ]:
        data = text.encode()
        version = _choose_version(len(data))
        size = 17 + 4 * version
        words = _codewords(data, version)
        matrix = encode(text)

        # Recover the mask from the format information, then walk the placement backwards.
        recovered_mask = None
        for mask in range(8):
            probe = [[False] * size for _ in range(size)]
            _place_format(probe, size, mask)
            if all(probe[8][i] == matrix[8][i] for i in range(6)):
                recovered_mask = mask
                break
        if recovered_mask is None:
            print(f"FAIL: no format information found for {text[:20]!r}")
            failures += 1
            continue

        taken = _reserved(size, version)
        positions = _data_positions(size, taken)
        bits = [
            1 if (matrix[row][col] != _mask(recovered_mask, row, col)) else 0
            for row, col in positions
        ]
        read_back = [
            int("".join(str(bit) for bit in bits[i:i + 8]), 2)
            for i in range(0, len(words) * 8, 8)
        ]
        # The two format copies must carry identical bits, and the "always dark" module must have
        # survived. Both are structural invariants a reader depends on and neither is visible in
        # the round-trip above: an off-by-one in either format strip leaves the data intact and
        # produces a code that scans as nothing.
        for i in range(15):
            first = (
                matrix[8][i] if i < 6
                else matrix[8][7] if i == 6
                else matrix[8][8] if i == 7
                else matrix[7][8] if i == 8
                else matrix[14 - i][8]
            )
            second = matrix[8][size - 1 - i] if i < 8 else matrix[size - 15 + i][8]
            if first != second:
                print(f"FAIL: format bit {i} differs between its two copies")
                failures += 1
                break
        if not matrix[size - 8][8]:
            print("FAIL: the always-dark module at (size-8, 8) was overwritten")
            failures += 1

        if read_back != words:
            print(f"FAIL: {text[:20]!r} did not round-trip through placement and masking")
            failures += 1
        elif len(positions) < len(words) * 8:
            print(f"FAIL: {text[:20]!r} has fewer data modules than codewords")
            failures += 1

    try:
        encode("A" * 107)
    except TooLong:
        pass
    else:
        print("FAIL: 107 bytes was accepted; version 5-L holds 106")
        failures += 1

    print("qr.py self-test: " + ("ok" if failures == 0 else f"{failures} failure(s)"))
    return 1 if failures else 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        return _self_test()
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    try:
        print(render(encode(argv[1])))
    except TooLong as error:
        print(f"qr: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
