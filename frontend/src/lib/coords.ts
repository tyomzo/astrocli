/*
 * Astronomical coordinate notation — USB-05.
 *
 * Right ascension is sexagesimal *hours* (`05:35:17.3`), declination is signed sexagesimal
 * *degrees* (`-05°23'28"`). The requirement is not decoration: an operator planning a session
 * reads those forms out of a catalog, an ephemeris or a planetarium app, and a UI that demanded
 * decimal degrees would make every target entry a mental conversion performed in the dark.
 *
 * # Parsing accepts more than it prints
 *
 * The printed form is one thing; the accepted forms are everything a catalog might hand over —
 * colons, spaces, `h m s`, `° ' "`, or a bare decimal. Rejecting `5 35 17.3` because the app
 * prints colons would be a rule that exists only for the app's benefit.
 *
 * A bare number is the one genuinely ambiguous case, and it resolves by unit rather than by
 * guessing: `5.5` in the RA field is 5.5 *hours*, in the DEC field 5.5 *degrees*. Each field
 * knows what it holds, so nothing has to be inferred from the value.
 *
 * # Failure says what is wrong, not that something is
 *
 * Every rejection names the rule it broke ("right ascension runs 0h to 24h"), because the
 * operator is standing outdoors correcting a typo on a phone keyboard, and "invalid input" costs
 * them a guess per attempt.
 */

export type Parsed = { ok: true; value: number } | { ok: false; problem: string };

/** `05:35:17.3` — RA to a tenth of a second of time, which is ~1.5 arcseconds. */
export function formatRaHours(hours: number): string {
  const total = ((hours % 24) + 24) % 24;
  const h = Math.floor(total);
  const minutesTotal = (total - h) * 60;
  const m = Math.floor(minutesTotal);
  const s = (minutesTotal - m) * 60;
  // Rounding can carry: 59.96 s must become the next minute, not `:59:60.0`.
  return carry(h, m, s, 1, 24, (hh, mm, ss) => `${pad(hh)}:${pad(mm)}:${padSeconds(ss, 1)}`);
}

/** `-05°23'28"` — DEC to the arcsecond, with the sign always printed. */
export function formatDecDegrees(degrees: number): string {
  const sign = degrees < 0 ? '-' : '+';
  const magnitude = Math.abs(degrees);
  const d = Math.floor(magnitude);
  const minutesTotal = (magnitude - d) * 60;
  const m = Math.floor(minutesTotal);
  const s = (minutesTotal - m) * 60;
  return carry(
    d,
    m,
    s,
    0,
    Number.POSITIVE_INFINITY,
    (dd, mm, ss) => `${sign}${pad(dd)}°${pad(mm)}'${padSeconds(ss, 0)}"`,
  );
}

/** `47.3°`, or an explicit unknown — a `null` altitude must never render as `0.0°`. */
export function formatDegrees(degrees: number | null, digits = 1): string {
  return degrees === null ? '—' : `${degrees.toFixed(digits)}°`;
}

/** `05:35:17.3` etc → hours in [0, 24). */
export function parseRaHours(text: string): Parsed {
  const parts = sexagesimal(text);
  if (!parts.ok) return parts;
  if (parts.negative) {
    return { ok: false, problem: 'right ascension is never negative — it runs 0h to 24h' };
  }
  if (parts.value >= 24) {
    return { ok: false, problem: 'right ascension runs 0h to 24h' };
  }
  return { ok: true, value: parts.value };
}

/** `-05°23'28"` etc → degrees in [-90, 90]. */
export function parseDecDegrees(text: string): Parsed {
  const parts = sexagesimal(text);
  if (!parts.ok) return parts;
  const value = parts.negative ? -parts.value : parts.value;
  if (value < -90 || value > 90) {
    return { ok: false, problem: 'declination runs -90° to +90°' };
  }
  return { ok: true, value };
}

type Sexagesimal = { ok: true; value: number; negative: boolean } | { ok: false; problem: string };

/**
 * One to three numeric groups → a magnitude, plus the sign as a separate fact.
 *
 * The sign is separate because it cannot be recovered from the first group: `-0:30:00` is half a
 * degree south, and `-0` is `0`. Reading it off the text is the only way that case survives.
 */
function sexagesimal(text: string): Sexagesimal {
  const trimmed = text.trim();
  if (trimmed === '') {
    return { ok: false, problem: 'enter a coordinate' };
  }

  const negative = trimmed.startsWith('-');
  const groups = trimmed
    .replace(/^[+-]/, '')
    // Every separator a catalog might use, including the primes an ephemeris prints.
    .replace(/[hdms°'"′″:]/gi, ' ')
    .trim()
    .split(/\s+/);

  if (groups.length > 3) {
    return { ok: false, problem: 'expected up to three groups: degrees, minutes, seconds' };
  }

  const numbers: number[] = [];
  for (const group of groups) {
    // `Number` accepts '', '0x10' and 'Infinity'; none of those is a coordinate.
    if (!/^\d+(\.\d+)?$/.test(group)) {
      return { ok: false, problem: `"${group}" is not a number` };
    }
    numbers.push(Number(group));
  }

  const [whole = 0, minutes = 0, seconds = 0] = numbers;
  if (minutes >= 60) {
    return { ok: false, problem: 'minutes run 0 to 59' };
  }
  if (seconds >= 60) {
    return { ok: false, problem: 'seconds run 0 to 59' };
  }
  // Minutes and seconds are only meaningful as subdivisions, so a fractional whole cannot be
  // combined with them: `5.5:30` has no reading anyone intends.
  if (numbers.length > 1 && !Number.isInteger(whole)) {
    return { ok: false, problem: 'use either a decimal value or whole units with minutes' };
  }

  return { ok: true, value: whole + minutes / 60 + seconds / 3600, negative };
}

/**
 * Apply the carry that rounding the seconds can force.
 *
 * `59.96` printed to one decimal is `60.0`, and `05:35:60.0` is not a time. Handled once, here,
 * rather than in each formatter, because it is the kind of defect that shows up at one instant in
 * sixty and is never reproduced on demand.
 */
function carry(
  whole: number,
  minutes: number,
  seconds: number,
  digits: number,
  wrap: number,
  render: (whole: number, minutes: number, seconds: number) => string,
): string {
  let s = Number(seconds.toFixed(digits));
  let m = minutes;
  let w = whole;
  if (s >= 60) {
    s = 0;
    m += 1;
  }
  if (m >= 60) {
    m = 0;
    w += 1;
  }
  if (w >= wrap) {
    w -= wrap;
  }
  return render(w, m, s);
}

function pad(value: number): string {
  return value.toString().padStart(2, '0');
}

function padSeconds(value: number, digits: number): string {
  const text = value.toFixed(digits);
  return value < 10 ? `0${text}` : text;
}
