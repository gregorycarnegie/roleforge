//! A drawn card for alerts whose feed shipped no image of its own. The palette
//! follows the score, the shapes follow a hash of the posting, so every job
//! gets its own card and the same job always gets the same one.
//!
//! The PNG is hand-rolled: stored deflate blocks, no row filtering. That costs
//! roughly the raw pixel count in bytes and saves an image crate and a zlib
//! crate.
//!
//! ponytail: uncompressed PNG at ~400 kB a card, add a real deflate only if
//! Telegram upload time ever shows up.

use sha2::{Digest, Sha256};

const WIDTH: usize = 600;
const HEIGHT: usize = 314;

/// xorshift64*, seeded from the posting so the card is reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: &str) -> Self {
        let digest = Sha256::digest(seed.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        // Zero is the one state xorshift cannot leave.
        Self(u64::from_le_bytes(bytes) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A fraction in `0.0..1.0`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn between(&mut self, low: f64, high: f64) -> f64 {
        low + self.unit() * (high - low)
    }
}

fn hsv(hue: f64, saturation: f64, value: f64) -> [f64; 3] {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let chroma = value * saturation;
    let second = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let [red, green, blue] = match hue as u8 {
        0 => [chroma, second, 0.0],
        1 => [second, chroma, 0.0],
        2 => [0.0, chroma, second],
        3 => [0.0, second, chroma],
        4 => [second, 0.0, chroma],
        _ => [chroma, 0.0, second],
    };
    let base = value - chroma;
    [red + base, green + base, blue + base]
}

fn mix(from: [f64; 3], to: [f64; 3], amount: f64) -> [f64; 3] {
    [
        from[0] + (to[0] - from[0]) * amount,
        from[1] + (to[1] - from[1]) * amount,
        from[2] + (to[2] - from[2]) * amount,
    ]
}

/// Hot for a strong match, cool for a routine one, matching the alert's own
/// tier emoji.
fn hue(score: i64) -> f64 {
    match score {
        80.. => 12.0,
        60..=79 => 275.0,
        _ => 190.0,
    }
}

/// RGB pixels, row major: a dark ground, a corner glow, a few bright blobs and
/// faint diagonal ruling, all sized and placed from the seed.
fn draw(seed: &str, score: i64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let base = hue(score) + rng.between(-15.0, 15.0);
    let dark = hsv(
        base + rng.between(-25.0, 5.0),
        rng.between(0.6, 0.85),
        rng.between(0.08, 0.16),
    );
    let light = hsv(
        base + rng.between(-10.0, 30.0),
        rng.between(0.6, 0.9),
        rng.between(0.8, 1.0),
    );
    // The glow sits in one of the four corners, pulled a little inwards.
    let (glow_x, glow_y) = (
        rng.between(0.1, 0.35) + f64::from(rng.next().is_multiple_of(2)) * 0.55,
        rng.between(0.1, 0.35) + f64::from(rng.next().is_multiple_of(2)) * 0.55,
    );
    let (glow_x, glow_y) = (glow_x * WIDTH as f64, glow_y * HEIGHT as f64);
    let reach = rng.between(0.7, 1.1) * (WIDTH as f64).hypot(HEIGHT as f64);
    let blobs: Vec<(f64, f64, f64, [f64; 3], f64)> = (0..3 + (rng.next() % 3) as usize)
        .map(|_| {
            (
                rng.between(-0.1, 1.1) * WIDTH as f64,
                rng.between(-0.1, 1.1) * HEIGHT as f64,
                rng.between(90.0, 260.0),
                hsv(
                    base + rng.between(-110.0, 110.0),
                    rng.between(0.45, 0.85),
                    rng.between(0.75, 1.0),
                ),
                rng.between(0.3, 0.7),
            )
        })
        .collect();
    let (slope, period) = (rng.between(-1.2, 1.2), rng.between(18.0, 46.0));

    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT * 3);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (x, y) = (x as f64, y as f64);
            let glow = 1.0 - ((x - glow_x).hypot(y - glow_y) / reach).clamp(0.0, 1.0);
            let mut colour = mix(dark, light, glow.powf(2.2));
            for (blob_x, blob_y, radius, tint, strength) in &blobs {
                let distance = (x - blob_x).hypot(y - blob_y);
                if distance < *radius {
                    // Squared falloff, so a blob reads as light, not as a disc.
                    let falloff = 1.0 - distance / radius;
                    colour = mix(colour, *tint, strength * falloff * falloff);
                }
            }
            if (x + y * slope).rem_euclid(period) < period * 0.35 {
                colour = mix(colour, [1.0, 1.0, 1.0], 0.05);
            }
            pixels.extend(colour.map(|channel| (channel.clamp(0.0, 1.0) * 255.0) as u8));
        }
    }

    // The score, bottom left, shadowed so it holds up over a bright blob.
    let scale = 10;
    let (left, top) = (40, HEIGHT as i64 - 7 * scale - 40);
    let number = score.to_string();
    stamp(
        &mut pixels,
        &number,
        left + 5,
        top + 5,
        scale,
        [0.0, 0.0, 0.0],
        0.45,
    );
    stamp(
        &mut pixels,
        &number,
        left,
        top,
        scale,
        [1.0, 1.0, 1.0],
        0.95,
    );
    pixels
}

/// 5x7 digits, one bit per pixel, five low bits per row. Only digits: the card
/// carries the score, the caption carries the words.
const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ],
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ],
    [
        0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ],
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ],
];

/// Blends one pixel towards `tint`, ignoring anything off canvas.
fn blend(pixels: &mut [u8], x: i64, y: i64, tint: [f64; 3], amount: f64) {
    if !(0..WIDTH as i64).contains(&x) || !(0..HEIGHT as i64).contains(&y) {
        return;
    }
    let at = (y as usize * WIDTH + x as usize) * 3;
    for channel in 0..3 {
        let was = f64::from(pixels[at + channel]) / 255.0;
        pixels[at + channel] = ((was + (tint[channel] - was) * amount) * 255.0) as u8;
    }
}

/// Stamps `number` at `scale`, each glyph pixel becoming a scale x scale block.
fn stamp(
    pixels: &mut [u8],
    number: &str,
    left: i64,
    top: i64,
    scale: i64,
    tint: [f64; 3],
    amount: f64,
) {
    for (index, digit) in number.bytes().filter(u8::is_ascii_digit).enumerate() {
        let glyph = DIGITS[usize::from(digit - b'0')];
        let origin = left + index as i64 * 6 * scale;
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (0b10000 >> column) == 0 {
                    continue;
                }
                for y in 0..scale {
                    for x in 0..scale {
                        let at_x = origin + (column as i64 * scale) + x;
                        let at_y = top + (row as i64 * scale) + y;
                        blend(pixels, at_x, at_y, tint, amount);
                    }
                }
            }
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut low, mut high) = (1u32, 0u32);
    for byte in bytes {
        low = (low + u32::from(*byte)) % 65521;
        high = (high + low) % 65521;
    }
    (high << 16) | low
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend(&body);
    out.extend(crc32(&body).to_be_bytes());
}

/// zlib wrapping uncompressed deflate blocks: valid, and about as much encoder
/// as a gradient deserves.
fn zlib(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for (index, block) in raw.chunks(0xffff).enumerate() {
        let last = u8::from((index + 1) * 0xffff >= raw.len());
        let length = block.len() as u16;
        out.push(last);
        out.extend(length.to_le_bytes());
        out.extend((!length).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend(adler32(raw).to_be_bytes());
    out
}

/// A PNG card for this posting, ready for Telegram's photo upload.
pub fn card(seed: &str, score: i64) -> Vec<u8> {
    let pixels = draw(seed, score);
    let mut raw = Vec::with_capacity(pixels.len() + HEIGHT);
    for row in pixels.chunks(WIDTH * 3) {
        // Filter type 0: none.
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut header = Vec::with_capacity(13);
    header.extend((WIDTH as u32).to_be_bytes());
    header.extend((HEIGHT as u32).to_be_bytes());
    // 8 bits per channel, truecolour, no interlace.
    header.extend([8, 2, 0, 0, 0]);

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib(&raw));
    chunk(&mut png, b"IEND", &[]);
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_is_a_png_that_differs_per_posting() {
        let one = card("https://example.com/jobs/1", 87);
        assert_eq!(&one[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(&one[12..16], b"IHDR");
        assert_eq!(&one[one.len() - 12..], b"\0\0\0\0IEND\xae\x42\x60\x82");
        // Dimensions, as the header states them.
        assert_eq!(&one[16..24], [0, 0, 2, 88, 0, 0, 1, 58]);
        // Same posting, same card; different posting, different card.
        assert_eq!(one, card("https://example.com/jobs/1", 87));
        assert_ne!(one, card("https://example.com/jobs/2", 87));
        assert_ne!(one, card("https://example.com/jobs/1", 30));
    }

    #[test]
    fn the_score_is_stamped_on_the_card() {
        // Same seed, same artwork: only the stamped digits can differ.
        let seed = "https://example.com/jobs/1";
        assert_ne!(card(seed, 87), card(seed, 88));

        let mut pixels = vec![0u8; WIDTH * HEIGHT * 3];
        stamp(&mut pixels, "1", 0, 0, 1, [1.0, 1.0, 1.0], 1.0);
        // Row 0 of the glyph for 1 is 00100: the third column, and only it.
        assert_eq!(
            &pixels[..15],
            [0, 0, 0, 0, 0, 0, 255, 255, 255, 0, 0, 0, 0, 0, 0]
        );
        // Non-digits are skipped rather than indexed into the glyph table.
        stamp(&mut pixels, "%", 0, 0, 1, [0.0, 0.0, 0.0], 1.0);
        assert_eq!(pixels[6], 255);
    }

    #[test]
    fn checksums_match_the_reference_values() {
        // The standard check values for both algorithms.
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }
}
