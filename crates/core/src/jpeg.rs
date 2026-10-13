//! Lossless Huffman optimization for baseline JPEG.
//!
//! `jpeg-encoder` can build optimal Huffman tables itself, but turning that on
//! also switches it from one interleaved scan to three single-component scans
//! (`encoder.rs:589` forces the sequential path, with no way to opt out). That
//! layout is legal baseline JPEG and libjpeg reads it, but it is rare enough
//! that simpler decoders mishandle it — including `zune-jpeg`, which the
//! `image` crate uses, which returns noise for every such file.
//!
//! The two things are separable. Optimal tables are what `cjpeg -optimize`,
//! mozjpeg and Pillow's `optimize=True` all produce, and they keep the ordinary
//! interleaved scan. So rather than let the encoder do it, this module takes the
//! finished interleaved file and rewrites just its Huffman coding:
//!
//! 1. decode the entropy-coded scan into its symbols — no dequantization, no
//!    IDCT, so nothing is approximated — and count them;
//! 2. build optimal tables from those counts per Annex K;
//! 3. decode the scan again and re-encode the identical symbols against the
//!    new tables.
//!
//! The scan is decoded a block at a time, each time, so that what this holds
//! is the file it writes and not the symbols of the file it reads.
//!
//! Everything else in the file is copied through untouched: SOF, DQT, the scan
//! header, component order, sampling factors, and every DCT coefficient of
//! the image. Only blocks past the image's edge, which no decoder shows, are
//! coded afresh, as libjpeg codes them: see `Blocks`. The result
//! decodes to the same pixels, byte for byte, and differs only in the DHT
//! segments and the entropy bits. It is the same transformation
//! `jpegtran -optimize` performs on a file libjpeg wrote.
//!
//! Anything unexpected — progressive, restart markers, multiple scans, 12-bit —
//! returns `None`, and the caller keeps the original file.

/// One entropy-coded symbol together with the raw magnitude bits that follow
/// it. Re-encoding replaces the symbol's code and copies `extra` verbatim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sym {
    /// `class << 2 | id`, identifying which of the eight possible tables codes
    /// this symbol.
    slot: u8,
    symbol: u8,
    extra: u16,
    extra_len: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Table {
    bits: [u8; 16],
    values: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Component {
    id: u8,
    h: u32,
    v: u32,
    dc: u8,
    ac: u8,
}

struct Parsed<'a> {
    /// Every segment before SOS except the DHTs, in file order.
    head: Vec<&'a [u8]>,
    sos: &'a [u8],
    entropy: &'a [u8],
    width: u32,
    height: u32,
    /// Frame components in scan order.
    components: Vec<Component>,
    tables: Vec<Option<Table>>,
}

/// Rewrite `jpeg`'s Huffman tables to be optimal for its own content.
///
/// Returns `None` if the file is not a shape this can handle, or if the result
/// would not be smaller, or if the rewritten file does not decode back to the
/// exact symbol stream it was built from.
pub fn optimize_huffman(jpeg: &[u8]) -> Option<Vec<u8>> {
    macro_rules! stage {
        ($e:expr, $what:literal) => {
            match $e {
                Some(v) => v,
                None => {
                    if std::env::var_os("EPUBKIT_JPEG_TRACE").is_some() {
                        eprintln!("optimize_huffman: gave up at {}", $what);
                    }
                    return None;
                }
            }
        };
    }

    let parsed = stage!(parse(jpeg), "parse");

    // The scan is read a block at a time, three times over: to count its
    // symbols, to code them against the tables those counts make, and to
    // check what was written. A list of all its symbols, copied to code the
    // padding afresh, beside another read back from the rewritten file to
    // check it, held ten times a JPEG of colour noise.
    let mut counts = [[0u32; 256]; 8];
    let mut any = false;
    let mut blocks = stage!(Blocks::new(&parsed, true), "decode");
    while let Some(block) = stage!(blocks.next(), "decode") {
        for sym in block {
            counts[usize::from(sym.slot)][usize::from(sym.symbol)] += 1;
            any = true;
        }
    }
    if !any {
        return None;
    }

    let tables = stage!(build_tables(&parsed, &counts), "table building");
    // No rewrite at all where it would be no smaller than the file.
    let out = stage!(rewrite(&parsed, &tables, jpeg.len()), "encode")?;

    // Decode what was just written and require the same symbols back, block
    // for block, as they were coded. This is cheap — no IDCT is involved —
    // and it means a bug here degrades to "no saving" rather than to a
    // corrupt book.
    {
        let check = stage!(parse(&out), "re-parse of the rewritten file");
        let mut ours = stage!(Blocks::new(&parsed, true), "decode");
        let mut theirs = stage!(
            Blocks::new(&check, false),
            "re-decode of the rewritten file"
        );
        loop {
            let coded = stage!(ours.next(), "decode");
            let read = stage!(theirs.next(), "re-decode of the rewritten file");
            match (coded, read) {
                (None, None) => break,
                (Some(coded), Some(read)) if coded == read => {}
                _ => stage!(
                    None::<()>,
                    "verification: the rewritten scan decodes to different symbols"
                ),
            }
        }
    }

    Some(out)
}

// ------------------------------------------------------------------ parsing

fn be16(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as usize)
}

fn parse(jpeg: &[u8]) -> Option<Parsed<'_>> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }

    let mut head = Vec::new();
    let mut tables: Vec<Option<Table>> = vec![None; 8];
    let mut frame: Option<(u32, u32, Vec<Component>)> = None;
    let mut at = 2;

    let (sos, entropy_start) = loop {
        if *jpeg.get(at)? != 0xFF {
            return None;
        }
        // Fill bytes are legal between segments.
        while jpeg.get(at + 1) == Some(&0xFF) {
            at += 1;
        }

        let marker = *jpeg.get(at + 1)?;
        // Standalone markers have no length and belong nowhere here.
        if marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            return None;
        }

        let length = be16(jpeg, at + 2)?;
        let segment = jpeg.get(at..at + 2 + length)?;
        let payload = jpeg.get(at + 4..at + 2 + length)?;

        match marker {
            // Baseline only. SOF2 (progressive) and the arithmetic-coded and
            // lossless variants are all out of scope.
            0xC0 => {
                frame = Some(parse_frame(payload)?);
                head.push(segment);
            }
            // The one segment deliberately not carried over: these are the
            // tables being replaced.
            0xC4 => parse_tables(payload, &mut tables)?,
            0xDD => {
                // A restart interval would put RST markers in the scan.
                if be16(payload, 0)? != 0 {
                    return None;
                }
                head.push(segment);
            }
            0xDA => break (segment, at + 2 + length),
            0xC1..=0xCF => return None,
            _ => head.push(segment),
        }

        at += 2 + length;
    };

    let (width, height, mut components) = frame?;
    let order = parse_scan(sos.get(4..)?, &mut components)?;
    let components: Vec<Component> = order.iter().map(|&i| components[i]).collect();

    // The entropy-coded data runs to the next real marker. A restart marker
    // here would mean the scan is segmented; another SOS would mean the file
    // has more scans than this handles. Either way, leave the file alone.
    let mut end = entropy_start;
    loop {
        if *jpeg.get(end)? == 0xFF {
            match *jpeg.get(end + 1)? {
                0x00 => end += 2,
                0xFF => end += 1,
                0xD9 => break,
                _ => return None,
            }
        } else {
            end += 1;
        }
    }
    // Nothing may follow EOI.
    if jpeg.len() != end + 2 {
        return None;
    }

    Some(Parsed {
        head,
        sos,
        entropy: &jpeg[entropy_start..end],
        width,
        height,
        components,
        tables,
    })
}

fn parse_frame(payload: &[u8]) -> Option<(u32, u32, Vec<Component>)> {
    if *payload.first()? != 8 {
        return None; // 12-bit samples
    }
    let height = be16(payload, 1)? as u32;
    let width = be16(payload, 3)? as u32;
    let count = *payload.get(5)? as usize;
    if count == 0 || count > 4 || payload.len() < 6 + count * 3 {
        return None;
    }

    let mut components = Vec::with_capacity(count);
    for i in 0..count {
        let at = 6 + i * 3;
        let hv = payload[at + 1];
        let (h, v) = ((hv >> 4) as u32, (hv & 15) as u32);
        if h == 0 || v == 0 || h > 4 || v > 4 {
            return None;
        }
        components.push(Component {
            id: payload[at],
            // A single-component frame is coded as one block per MCU
            // regardless of what the sampling factors claim.
            h: if count == 1 { 1 } else { h },
            v: if count == 1 { 1 } else { v },
            dc: 0,
            ac: 0,
        });
    }

    Some((width, height, components))
}

fn parse_tables(mut payload: &[u8], tables: &mut [Option<Table>]) -> Option<()> {
    while !payload.is_empty() {
        let class_and_id = *payload.first()?;
        let (class, id) = ((class_and_id >> 4) as usize, (class_and_id & 15) as usize);
        if class > 1 || id > 3 {
            return None;
        }

        let mut bits = [0u8; 16];
        bits.copy_from_slice(payload.get(1..17)?);
        let count: usize = bits.iter().map(|&b| b as usize).sum();
        let values = payload.get(17..17 + count)?.to_vec();

        tables[class * 4 + id] = Some(Table { bits, values });
        payload = &payload[17 + count..];
    }
    Some(())
}

/// Reads the scan header, records each component's table assignment, and
/// returns the frame-component indices in scan order.
fn parse_scan(payload: &[u8], components: &mut [Component]) -> Option<Vec<usize>> {
    let count = *payload.first()? as usize;
    // Only fully interleaved scans: every component present, in one pass.
    if count != components.len() {
        return None;
    }

    let mut order = Vec::with_capacity(count);
    for i in 0..count {
        let id = *payload.get(1 + i * 2)?;
        let tables = *payload.get(2 + i * 2)?;
        let index = components.iter().position(|c| c.id == id)?;
        if order.contains(&index) {
            return None;
        }
        components[index].dc = tables >> 4;
        components[index].ac = tables & 15;
        if components[index].dc > 3 || components[index].ac > 3 {
            return None;
        }
        order.push(index);
    }

    // Baseline spectral selection: the whole block, no successive approximation.
    let tail = payload.get(1 + count * 2..4 + count * 2)?;
    if tail != [0, 63, 0] {
        return None;
    }

    Some(order)
}

// ------------------------------------------------------------------ decoding

/// The bits of entropy-coded data, read ahead so that a code can be looked
/// up from the bits that begin it.
struct BitReader<'a> {
    data: &'a [u8],
    at: usize,
    /// Bits read ahead, the next of them the highest, and below them zeroes.
    ahead: u64,
    held: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            at: 0,
            ahead: 0,
            held: 0,
        }
    }

    /// Read ahead to at least 57 bits, or as far as the data goes.
    fn fill(&mut self) {
        while self.held <= 56 {
            let Some(&byte) = self.data.get(self.at) else {
                return;
            };
            if byte == 0xFF {
                // A stuffed zero means a literal 0xFF; anything else is a
                // marker, and the entropy data has ended.
                if self.data.get(self.at + 1) != Some(&0x00) {
                    return;
                }
                self.at += 2;
            } else {
                self.at += 1;
            }
            self.ahead |= u64::from(byte) << (56 - self.held);
            self.held += 8;
        }
    }

    /// The next sixteen bits, zeroes past the end of the data, and how many
    /// of them there are.
    fn peek(&mut self) -> (u32, u32) {
        if self.held < 16 {
            self.fill();
        }
        ((self.ahead >> 48) as u32, self.held.min(16))
    }

    fn skip(&mut self, count: u32) {
        self.ahead <<= count;
        self.held -= count;
    }

    fn bits(&mut self, count: u8) -> Option<u16> {
        let count = u32::from(count);
        if count == 0 {
            return Some(0);
        }
        if self.held < count {
            self.fill();
            if self.held < count {
                return None;
            }
        }
        let value = (self.ahead >> (64 - count)) as u16;
        self.skip(count);
        Some(value)
    }
}

/// How many bits ahead a code is looked up in one step, as libjpeg looks
/// codes up. Sought a length at a time instead, codes as long as colour
/// noise's took most of the time reading a scan takes.
const LOOKAHEAD: usize = 10;

/// Canonical decoding tables, per Annex F.2.2.3.
struct Decoder {
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    values: Vec<u8>,
    /// For each value the next `LOOKAHEAD` bits can take, what seeking the
    /// code they begin with finds: its length and its symbol, or no symbol
    /// where the table has none for it. A length of 0 where the code is
    /// longer.
    ahead: Vec<(u8, Option<u8>)>,
}

impl Decoder {
    fn new(table: &Table) -> Self {
        let mut decoder = Decoder {
            mincode: [0; 17],
            maxcode: [-1; 17],
            valptr: [0; 17],
            values: table.values.clone(),
            ahead: Vec::new(),
        };

        let mut code = 0i32;
        let mut index = 0usize;
        for length in 1..=16 {
            let count = table.bits[length - 1] as i32;
            if count > 0 {
                decoder.valptr[length] = index;
                decoder.mincode[length] = code;
                index += count as usize;
                code += count;
                decoder.maxcode[length] = code - 1;
            }
            code <<= 1;
        }

        decoder.ahead = (0..1 << LOOKAHEAD)
            .map(|bits: i32| {
                (1..=LOOKAHEAD)
                    .find_map(|length| decoder.seek(bits >> (LOOKAHEAD - length), length))
                    .unwrap_or((0, None))
            })
            .collect();
        decoder
    }

    /// What the code `code`, `length` bits long, is: its length and its
    /// symbol, or no symbol where the table has none for it. `None` where
    /// there is no code that long beginning so.
    fn seek(&self, code: i32, length: usize) -> Option<(u8, Option<u8>)> {
        (code <= self.maxcode[length]).then(|| {
            let symbol = usize::try_from(code - self.mincode[length])
                .ok()
                .and_then(|offset| self.values.get(self.valptr[length] + offset));
            (length as u8, symbol.copied())
        })
    }

    fn decode(&self, reader: &mut BitReader) -> Option<u8> {
        let (next, there) = reader.peek();
        let found = match self.ahead[(next >> (16 - LOOKAHEAD)) as usize] {
            (0, _) => (LOOKAHEAD + 1..=16)
                .find_map(|length| self.seek((next >> (16 - length)) as i32, length)),
            found => Some(found),
        };
        let (length, symbol) = found?;
        if u32::from(length) > there {
            return None;
        }
        reader.skip(u32::from(length));
        symbol
    }
}

/// The scan of a parsed JPEG, read a block at a time in the order it is
/// coded, so that no more than a block of its symbols is held.
///
/// `unpadded`, each block past its component's edge is read as libjpeg codes
/// it: its DC that of the block before, so a difference of zero, and no AC.
/// An interleaved scan codes whole MCUs, so where a component does not fill
/// out the last one, the encoder codes blocks past the image's edge that no
/// decoder shows; `jpeg-encoder` fills them by repeating the image's last row
/// or column, at a real cost in a dithered image. Every other block keeps its
/// coefficients, its DC difference worked out again from the block that now
/// comes before it.
struct Blocks<'a> {
    parsed: &'a Parsed<'a>,
    decoders: Vec<Option<Decoder>>,
    reader: BitReader<'a>,
    unpadded: bool,
    mcus_x: u32,
    mcus: u32,
    /// Each component's extent in blocks (A.1.1).
    extents: Vec<(u32, u32)>,
    /// Each component's DC as the scan has it, and as it is rewritten.
    written: Vec<i32>,
    rewritten: Vec<i32>,
    /// Where the next block is: its MCU, its component, and which of that
    /// component's blocks in the MCU it is.
    mcu: u32,
    component: usize,
    block: u32,
    /// The symbols of the block last read: its DC, and the AC that follow it
    /// up to an end of block or the 63rd coefficient, at most 63 of them.
    symbols: [Sym; 64],
}

impl<'a> Blocks<'a> {
    fn new(parsed: &'a Parsed<'a>, unpadded: bool) -> Option<Self> {
        let hmax = parsed.components.iter().map(|c| c.h).max()?;
        let vmax = parsed.components.iter().map(|c| c.v).max()?;
        let mcus_x = parsed.width.div_ceil(8 * hmax);
        let mcus_y = parsed.height.div_ceil(8 * vmax);
        let extents = parsed
            .components
            .iter()
            .map(|c| {
                let width = (parsed.width * c.h).div_ceil(hmax);
                let height = (parsed.height * c.v).div_ceil(vmax);
                (width.div_ceil(8), height.div_ceil(8))
            })
            .collect();
        Some(Self {
            parsed,
            decoders: parsed
                .tables
                .iter()
                .map(|t| t.as_ref().map(Decoder::new))
                .collect(),
            reader: BitReader::new(parsed.entropy),
            unpadded,
            mcus_x,
            mcus: mcus_x * mcus_y,
            extents,
            written: vec![0; parsed.components.len()],
            rewritten: vec![0; parsed.components.len()],
            mcu: 0,
            component: 0,
            block: 0,
            symbols: [Sym {
                slot: 0,
                symbol: 0,
                extra: 0,
                extra_len: 0,
            }; 64],
        })
    }

    /// The symbols of the next block, or `Some(None)` past the last. `None`
    /// where the scan does not read: a code no table has, or data that ends
    /// too soon.
    fn next(&mut self) -> Option<Option<&[Sym]>> {
        if self.mcu >= self.mcus {
            return Some(None);
        }
        let index = self.component;
        let component = *self.parsed.components.get(index)?;
        let dc_table = self.decoders.get(usize::from(component.dc))?.as_ref()?;
        let ac_slot = 4 + component.ac;
        let ac_table = self.decoders.get(usize::from(ac_slot))?.as_ref()?;

        // DC: one symbol giving the magnitude category.
        let category = dc_table.decode(&mut self.reader)?;
        if category > 15 {
            return None;
        }
        let dc = Sym {
            slot: component.dc,
            symbol: category,
            extra: self.reader.bits(category)?,
            extra_len: category,
        };
        self.symbols[0] = dc;
        let mut len = 1;

        // AC: run-length/size pairs until end-of-block or 63 coefficients.
        let mut k = 1;
        while k <= 63 {
            let rs = ac_table.decode(&mut self.reader)?;
            let (run, size) = (rs >> 4, rs & 15);
            *self.symbols.get_mut(len)? = Sym {
                slot: ac_slot,
                symbol: rs,
                extra: self.reader.bits(size)?,
                extra_len: size,
            };
            len += 1;

            if size == 0 {
                // 0xF0 is a run of sixteen zeroes; 0x00 ends the block.
                if run == 15 {
                    k += 16;
                    continue;
                }
                break;
            }
            k += u32::from(run) + 1;
        }

        if self.unpadded {
            let (mcu_x, mcu_y) = (self.mcu % self.mcus_x, self.mcu / self.mcus_x);
            let x = mcu_x * component.h + self.block % component.h;
            let y = mcu_y * component.v + self.block / component.h;
            let past_edge = x >= self.extents[index].0 || y >= self.extents[index].1;

            self.written[index] += extend(dc.extra, dc.extra_len);
            if past_edge {
                let nothing = Sym {
                    symbol: 0,
                    extra: 0,
                    extra_len: 0,
                    ..dc
                };
                self.symbols[0] = nothing;
                self.symbols[1] = Sym {
                    slot: ac_slot,
                    ..nothing
                };
                len = 2;
            } else {
                let (category, extra) = magnitude(self.written[index] - self.rewritten[index]);
                self.rewritten[index] = self.written[index];
                self.symbols[0] = Sym {
                    symbol: category,
                    extra,
                    extra_len: category,
                    ..dc
                };
            }
        }

        self.block += 1;
        if self.block == component.h * component.v {
            self.block = 0;
            self.component += 1;
            if self.component == self.parsed.components.len() {
                self.component = 0;
                self.mcu += 1;
            }
        }
        Some(Some(&self.symbols[..len]))
    }
}

/// The value `extra_len` magnitude bits stand for (F.2.2.1, EXTEND).
fn extend(extra: u16, extra_len: u8) -> i32 {
    if extra_len == 0 {
        0
    } else if i32::from(extra) < 1 << (extra_len - 1) {
        i32::from(extra) - (1 << extra_len) + 1
    } else {
        i32::from(extra)
    }
}

/// A difference's magnitude category and the bits that follow it (F.1.2.1).
fn magnitude(value: i32) -> (u8, u16) {
    let category = (32 - value.unsigned_abs().leading_zeros()) as u8;
    let extra = if value < 0 {
        value + (1 << category) - 1
    } else {
        value
    };
    (category, extra as u16)
}

// ------------------------------------------------------------ table building

/// Build the optimal table for one symbol distribution, per Annex K.2
/// (Figures K.1 through K.4).
fn optimal_table(counts: &[u32; 256]) -> Option<Table> {
    let mut freq = [0u32; 257];
    freq[..256].copy_from_slice(counts);
    // A dummy symbol that cannot appear in the data, reserving the all-ones
    // codeword so no real symbol is ever assigned it. Decoders are entitled to
    // treat that codeword as invalid.
    freq[256] = 1;

    let mut others = [-1i32; 257];
    let mut codesize = [0usize; 257];

    // Figure K.1: repeatedly merge the two least frequent symbols.
    loop {
        let least = |skip: usize| {
            let mut best = usize::MAX;
            let mut best_freq = u32::MAX;
            for (i, &f) in freq.iter().enumerate() {
                if f > 0 && f <= best_freq && i != skip {
                    best_freq = f;
                    best = i;
                }
            }
            best
        };

        let mut v1 = least(usize::MAX);
        let mut v2 = least(v1);
        if v2 == usize::MAX {
            break;
        }

        freq[v1] += freq[v2];
        freq[v2] = 0;

        codesize[v1] += 1;
        while others[v1] >= 0 {
            v1 = others[v1] as usize;
            codesize[v1] += 1;
        }
        others[v1] = v2 as i32;

        codesize[v2] += 1;
        while others[v2] >= 0 {
            v2 = others[v2] as usize;
            codesize[v2] += 1;
        }
    }

    // Figure K.2: how many codes of each length.
    let mut bits = [0u32; 33];
    for &size in codesize.iter() {
        if size > 0 {
            if size > 32 {
                return None;
            }
            bits[size] += 1;
        }
    }

    // Figure K.3: fold codes longer than 16 bits back into the tree.
    let mut i = 32;
    while i > 16 {
        while bits[i] > 0 {
            let mut j = i - 2;
            while bits[j] == 0 {
                j -= 1;
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
        i -= 1;
    }
    while bits[i] == 0 {
        i -= 1;
        if i == 0 {
            return None;
        }
    }
    // Drop the reserved dummy, which is by construction the longest code.
    bits[i] -= 1;

    // Figure K.4: symbols sorted by code length, then by value. The dummy at
    // index 256 is excluded by construction — only real symbols are listed.
    let mut values = Vec::new();
    for length in 1..=32 {
        for (symbol, &size) in codesize.iter().take(256).enumerate() {
            if size == length {
                values.push(symbol as u8);
            }
        }
    }

    let mut lengths = [0u8; 16];
    for (slot, count) in lengths.iter_mut().enumerate() {
        *count = u8::try_from(bits[slot + 1]).ok()?;
    }
    if lengths.iter().map(|&b| b as usize).sum::<usize>() != values.len() {
        return None;
    }

    Some(Table {
        bits: lengths,
        values,
    })
}

fn build_tables(parsed: &Parsed, counts: &[[u32; 256]; 8]) -> Option<Vec<Option<Table>>> {
    let mut tables = vec![None; 8];
    for (slot, counts) in counts.iter().enumerate() {
        if counts.iter().all(|&c| c == 0) {
            // Unused by this scan, so it need not be written at all.
            continue;
        }
        // If the optimal table cannot be built, keep the one already there
        // rather than losing the ability to code these symbols.
        tables[slot] = optimal_table(counts).or_else(|| parsed.tables[slot].clone());
        tables[slot].as_ref()?;
    }

    Some(tables)
}

// ------------------------------------------------------------------ encoding

/// Canonical code assignment, per Annex C.2.
fn codes_for(table: &Table) -> [(u8, u16); 256] {
    let mut codes = [(0u8, 0u16); 256];
    let mut code = 0u16;
    let mut index = 0usize;

    for length in 1..=16u8 {
        for _ in 0..table.bits[length as usize - 1] {
            if let Some(&symbol) = table.values.get(index) {
                codes[symbol as usize] = (length, code);
            }
            code = code.wrapping_add(1);
            index += 1;
        }
        code <<= 1;
    }

    codes
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    held: u32,
}

impl BitWriter {
    fn put(&mut self, code: u16, length: u8) {
        self.acc = (self.acc << length) | code as u32;
        self.held += length as u32;

        while self.held >= 8 {
            self.held -= 8;
            let byte = ((self.acc >> self.held) & 0xFF) as u8;
            self.out.push(byte);
            // A literal 0xFF in the entropy stream is stuffed so it cannot be
            // mistaken for a marker.
            if byte == 0xFF {
                self.out.push(0x00);
            }
        }
        self.acc &= (1u32 << self.held) - 1;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.held > 0 {
            // The standard pads the final byte with one bits.
            let pad = 8 - self.held as u8;
            self.put((1u16 << pad) - 1, pad);
        }
        self.out
    }
}

/// The file `parsed` again with `tables` in place of its own, and its scan,
/// its padding coded afresh, coded against them; `Some(None)` if that comes
/// to `limit` bytes or more, a rewrite no smaller than the original being of
/// no use. It is written into the one buffer, `limit` bytes long, and given
/// up on once it fills it. `None` where the scan does not read.
fn rewrite(parsed: &Parsed, tables: &[Option<Table>], limit: usize) -> Option<Option<Vec<u8>>> {
    let mut out = Vec::with_capacity(limit);
    out.extend_from_slice(&[0xFF, 0xD8]);
    for segment in &parsed.head {
        out.extend_from_slice(segment);
    }

    // One DHT per table, in class/id order, immediately before the scan.
    for (slot, table) in tables.iter().enumerate() {
        let Some(table) = table else { continue };
        let length = 2 + 1 + 16 + table.values.len();
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&(length as u16).to_be_bytes());
        out.push(((slot as u8 / 4) << 4) | (slot as u8 % 4));
        out.extend_from_slice(&table.bits);
        out.extend_from_slice(&table.values);
    }
    out.extend_from_slice(parsed.sos);

    let codes: Vec<Option<[(u8, u16); 256]>> =
        tables.iter().map(|t| t.as_ref().map(codes_for)).collect();
    let mut writer = BitWriter {
        out,
        acc: 0,
        held: 0,
    };
    let mut blocks = Blocks::new(parsed, true)?;
    while let Some(block) = blocks.next()? {
        if writer.out.len() >= limit {
            return Some(None);
        }
        for sym in block {
            let (length, code) = codes[usize::from(sym.slot)].as_ref()?[usize::from(sym.symbol)];
            debug_assert!(length > 0, "symbol {} has no code", sym.symbol);
            writer.put(code, length);
            if sym.extra_len > 0 {
                writer.put(sym.extra, sym.extra_len);
            }
        }
    }

    let mut out = writer.finish();
    out.extend_from_slice(&[0xFF, 0xD9]);
    Some((out.len() < limit).then_some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dummy symbol has to keep the all-ones codeword out of the table.
    #[test]
    fn the_all_ones_codeword_is_never_assigned() {
        let mut counts = [0u32; 256];
        for (i, count) in counts.iter_mut().enumerate().take(40) {
            *count = (i as u32 + 1) * 7;
        }

        let table = optimal_table(&counts).unwrap();
        let codes = codes_for(&table);

        for &symbol in &table.values {
            let (length, code) = codes[symbol as usize];
            assert_ne!(
                code,
                (1u16 << length) - 1,
                "symbol {symbol} was given the all-ones code of length {length}"
            );
        }
    }

    /// Skewed distributions are exactly where naive construction overflows
    /// sixteen bits; Figure K.3 has to pull them back.
    #[test]
    fn no_code_is_longer_than_sixteen_bits() {
        let mut counts = [0u32; 256];
        counts[0] = 1_000_000;
        for (i, count) in counts.iter_mut().enumerate().skip(1).take(60) {
            // Fibonacci-ish frequencies force a deep, lopsided tree.
            *count = 1 << (i.min(30) as u32 / 2);
        }

        let table = optimal_table(&counts).unwrap();
        assert_eq!(table.bits.len(), 16);
        assert_eq!(
            table.bits.iter().map(|&b| b as usize).sum::<usize>(),
            table.values.len()
        );
    }

    /// A table with one live symbol is legal and must still round-trip.
    #[test]
    fn a_single_symbol_still_gets_a_code() {
        let mut counts = [0u32; 256];
        counts[42] = 900;

        let table = optimal_table(&counts).unwrap();
        assert_eq!(table.values, vec![42]);
        assert_eq!(codes_for(&table)[42].0, 1);
    }

    #[test]
    fn the_writer_stuffs_literal_ff_bytes() {
        let mut writer = BitWriter {
            out: Vec::new(),
            acc: 0,
            held: 0,
        };
        writer.put(0xFF, 8);
        assert_eq!(writer.finish(), vec![0xFF, 0x00]);
    }

    #[test]
    fn the_reader_unstuffs_them_again() {
        let mut reader = BitReader::new(&[0xFF, 0x00]);
        assert_eq!(reader.bits(8), Some(0xFF));
    }

    #[test]
    fn a_marker_ends_the_entropy_data() {
        let mut reader = BitReader::new(&[0xFF, 0xD9]);
        assert_eq!(reader.bits(1), None);
        let mut reader = BitReader::new(&[0xA5, 0xFF, 0x00, 0x3C, 0xFF, 0xD9, 0x12]);
        assert_eq!(reader.bits(4), Some(0xA));
        assert_eq!(reader.bits(12), Some(0x5FF));
        assert_eq!(reader.bits(4), Some(0x3));
        assert_eq!(
            reader.bits(5),
            None,
            "only four bits are left before the marker"
        );
        assert_eq!(reader.bits(4), Some(0xC));
    }

    /// A code is read from the bits ahead, as far as they go: one that ends
    /// before the data does is read, and one that would run past it is not.
    #[test]
    fn a_code_is_read_only_as_far_as_the_data_goes() {
        let mut bits = [0u8; 16];
        // One code of one bit, `0`, then one of three, `100`.
        bits[0] = 1;
        bits[2] = 1;
        let decoder = Decoder::new(&Table {
            bits,
            values: vec![7, 9],
        });
        let mut reader = BitReader::new(&[0b0100_0100, 0xFF, 0xD9]);
        assert_eq!(decoder.decode(&mut reader), Some(7));
        assert_eq!(decoder.decode(&mut reader), Some(9));
        assert_eq!(decoder.decode(&mut reader), Some(7));
        assert_eq!(decoder.decode(&mut reader), Some(9));
        assert_eq!(decoder.decode(&mut reader), None, "the data has ended");
        let mut reader = BitReader::new(&[0b1000_0000, 0x00]);
        assert_eq!(decoder.decode(&mut reader), Some(9));
        assert_eq!(decoder.decode(&mut reader), Some(7));
        assert_eq!(decoder.decode(&mut reader), Some(7));
    }

    /// An interleaved scan codes whole MCUs, so where an image's luma does
    /// not fill out the last MCU, it codes blocks past the image's edge that
    /// no decoder shows. libjpeg codes each as its DC unchanged and no AC;
    /// `jpeg-encoder` repeats the image's last row or column into them, at a
    /// cost of a quarter of the size of a strip eight pixels high.
    #[test]
    fn blocks_past_the_edge_are_coded_as_nothing() {
        // 4-level noise, so a block repeating the edge has AC to code.
        let (width, height) = (40u16, 24u16);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rgb = Vec::new();
        for _ in 0..width as usize * height as usize {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let v = [0u8, 85, 170, 255][(seed >> 33) as usize % 4];
            rgb.extend_from_slice(&[v, v, v]);
        }
        let mut plain = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut plain, 70);
        encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
        encoder
            .encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb)
            .unwrap();

        let rewritten = optimize_huffman(&plain).expect("rewritten");
        let parsed = parse(&rewritten).unwrap();
        let mut blocks = Blocks::new(&parsed, false).unwrap();

        // Walk the blocks as they are coded, and look at the luma ones past
        // the image's edge: 40 x 24 is coded as 48 x 32.
        let mut padding = 0;
        for mcu in 0..(3 * 2) {
            let (mcu_x, mcu_y) = (mcu % 3, mcu / 3);
            for (index, component) in parsed.components.iter().enumerate() {
                for block in 0..component.h * component.v {
                    let x = mcu_x * component.h + block % component.h;
                    let y = mcu_y * component.v + block / component.h;
                    let coded = blocks.next().unwrap().unwrap();

                    if index == 0 && (x >= 5 || y >= 3) {
                        padding += 1;
                        let symbols: Vec<u8> = coded.iter().map(|sym| sym.symbol).collect();
                        assert_eq!(
                            symbols,
                            [0, 0],
                            "block ({x}, {y}) is past the edge: a DC difference of 0 and an end of block"
                        );
                    }
                }
            }
        }
        assert_eq!(padding, 6 * 4 - 5 * 3, "luma blocks past the edge");
        assert!(blocks.next().unwrap().is_none(), "every block was walked");
    }

    #[test]
    fn rubbish_is_declined_rather_than_mangled() {
        assert!(optimize_huffman(b"not a jpeg at all").is_none());
        assert!(optimize_huffman(&[0xFF, 0xD8]).is_none());
        assert!(optimize_huffman(&[]).is_none());
    }
}
