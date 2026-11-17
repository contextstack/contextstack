//! `fpconv_dtoa` as Ruby's json 2.18 vendors it (`ext/json/ext/vendor/fpconv.c`, from
//! night-shift/fpconv, modified to append `.0` to plain integers): Grisu2 digits, which are
//! not always the shortest, so Rust's own float formatting cannot stand in for it.

// The tables are fpconv.c's, digit for digit.
#![allow(clippy::unreadable_literal)]

const NPOWERS: i32 = 87;
const STEPPOWERS: i32 = 8;
const FIRSTPOWER: i32 = -348;
const EXPMAX: i32 = -32;
const EXPMIN: i32 = -60;

const FRACMASK: u64 = 0x000F_FFFF_FFFF_FFFF;
const EXPMASK: u64 = 0x7FF0_0000_0000_0000;
const HIDDENBIT: u64 = 0x0010_0000_0000_0000;
const SIGNMASK: u64 = 0x8000_0000_0000_0000;
const EXPBIAS: i32 = 1023 + 52;

#[derive(Clone, Copy)]
struct Fp {
    frac: u64,
    exp: i32,
}

const fn fp(frac: u64, exp: i32) -> Fp {
    Fp { frac, exp }
}

#[rustfmt::skip]
const POWERS_TEN: [Fp; 87] = [
    fp(18054884314459144840, -1220), fp(13451937075301367670, -1193),
    fp(10022474136428063862, -1166), fp(14934650266808366570, -1140),
    fp(11127181549972568877, -1113), fp(16580792590934885855, -1087),
    fp(12353653155963782858, -1060), fp(18408377700990114895, -1034),
    fp(13715310171984221708, -1007), fp(10218702384817765436, -980),
    fp(15227053142812498563, -954),  fp(11345038669416679861, -927),
    fp(16905424996341287883, -901),  fp(12595523146049147757, -874),
    fp(9384396036005875287,  -847),  fp(13983839803942852151, -821),
    fp(10418772551374772303, -794),  fp(15525180923007089351, -768),
    fp(11567161174868858868, -741),  fp(17236413322193710309, -715),
    fp(12842128665889583758, -688),  fp(9568131466127621947,  -661),
    fp(14257626930069360058, -635),  fp(10622759856335341974, -608),
    fp(15829145694278690180, -582),  fp(11793632577567316726, -555),
    fp(17573882009934360870, -529),  fp(13093562431584567480, -502),
    fp(9755464219737475723,  -475),  fp(14536774485912137811, -449),
    fp(10830740992659433045, -422),  fp(16139061738043178685, -396),
    fp(12024538023802026127, -369),  fp(17917957937422433684, -343),
    fp(13349918974505688015, -316),  fp(9946464728195732843,  -289),
    fp(14821387422376473014, -263),  fp(11042794154864902060, -236),
    fp(16455045573212060422, -210),  fp(12259964326927110867, -183),
    fp(18268770466636286478, -157),  fp(13611294676837538539, -130),
    fp(10141204801825835212, -103),  fp(15111572745182864684, -77),
    fp(11258999068426240000, -50),   fp(16777216000000000000, -24),
    fp(12500000000000000000,   3),   fp(9313225746154785156,   30),
    fp(13877787807814456755,  56),   fp(10339757656912845936,  83),
    fp(15407439555097886824, 109),   fp(11479437019748901445, 136),
    fp(17105694144590052135, 162),   fp(12744735289059618216, 189),
    fp(9495567745759798747,  216),   fp(14149498560666738074, 242),
    fp(10542197943230523224, 269),   fp(15709099088952724970, 295),
    fp(11704190886730495818, 322),   fp(17440603504673385349, 348),
    fp(12994262207056124023, 375),   fp(9681479787123295682,  402),
    fp(14426529090290212157, 428),   fp(10748601772107342003, 455),
    fp(16016664761464807395, 481),   fp(11933345169920330789, 508),
    fp(17782069995880619868, 534),   fp(13248674568444952270, 561),
    fp(9871031767461413346,  588),   fp(14708983551653345445, 614),
    fp(10959046745042015199, 641),   fp(16330252207878254650, 667),
    fp(12166986024289022870, 694),   fp(18130221999122236476, 720),
    fp(13508068024458167312, 747),   fp(10064294952495520794, 774),
    fp(14996968138956309548, 800),   fp(11173611982879273257, 827),
    fp(16649979327439178909, 853),   fp(12405201291620119593, 880),
    fp(9242595204427927429,  907),   fp(13772540099066387757, 933),
    fp(10261342003245940623, 960),   fp(15290591125556738113, 986),
    fp(11392378155556871081, 1013),  fp(16975966327722178521, 1039),
    fp(12648080533535911531, 1066),
];

#[rustfmt::skip]
const TENS: [u64; 20] = [
    10000000000000000000, 1000000000000000000, 100000000000000000,
    10000000000000000, 1000000000000000, 100000000000000,
    10000000000000, 1000000000000, 100000000000,
    10000000000, 1000000000, 100000000,
    10000000, 1000000, 100000,
    10000, 1000, 100,
    10, 1,
];

fn find_cachedpow10(exp: i32, k: &mut i32) -> Fp {
    let one_log_ten = 0.301_029_995_663_981_14_f64;
    // C: (int)(-(exp + npowers) * one_log_ten), truncating toward zero.
    #[allow(clippy::cast_possible_truncation)]
    let approx = (f64::from(-(exp + NPOWERS)) * one_log_ten) as i32;
    let mut idx = (approx - FIRSTPOWER) / STEPPOWERS;
    loop {
        let entry = POWERS_TEN[usize::try_from(idx).unwrap_or(0)];
        let current = exp + entry.exp + 64;
        if current < EXPMIN {
            idx += 1;
            continue;
        }
        if current > EXPMAX {
            idx -= 1;
            continue;
        }
        *k = FIRSTPOWER + idx * STEPPOWERS;
        return entry;
    }
}

fn build_fp(d: f64) -> Fp {
    let bits = d.to_bits();
    let raw_exp = i32::try_from((bits & EXPMASK) >> 52).unwrap_or(0);
    let frac = bits & FRACMASK;
    if raw_exp == 0 {
        fp(frac, -EXPBIAS + 1)
    } else {
        fp(frac + HIDDENBIT, raw_exp - EXPBIAS)
    }
}

fn normalize(f: &mut Fp) {
    while f.frac & HIDDENBIT == 0 {
        f.frac <<= 1;
        f.exp -= 1;
    }
    let shift = 64 - 52 - 1;
    f.frac <<= shift;
    f.exp -= shift;
}

fn normalized_boundaries(f: Fp) -> (Fp, Fp) {
    let mut upper = fp((f.frac << 1) + 1, f.exp - 1);
    while upper.frac & (HIDDENBIT << 1) == 0 {
        upper.frac <<= 1;
        upper.exp -= 1;
    }
    let u_shift = 64 - 52 - 2;
    upper.frac <<= u_shift;
    upper.exp -= u_shift;
    let l_shift = if f.frac == HIDDENBIT { 2 } else { 1 };
    let mut lower = fp((f.frac << l_shift) - 1, f.exp - l_shift);
    lower.frac <<= lower.exp - upper.exp;
    lower.exp = upper.exp;
    (lower, upper)
}

#[allow(clippy::similar_names)] // fpconv.c's names: the four 32-bit partial products.
fn multiply(a: Fp, b: Fp) -> Fp {
    let lomask: u64 = 0x0000_0000_FFFF_FFFF;
    let ah_bl = (a.frac >> 32) * (b.frac & lomask);
    let al_bh = (a.frac & lomask) * (b.frac >> 32);
    let al_bl = (a.frac & lomask) * (b.frac & lomask);
    let ah_bh = (a.frac >> 32) * (b.frac >> 32);
    let mut tmp = (ah_bl & lomask) + (al_bh & lomask) + (al_bl >> 32);
    tmp += 1 << 31;
    fp(
        ah_bh + (ah_bl >> 32) + (al_bh >> 32) + (tmp >> 32),
        a.exp + b.exp + 64,
    )
}

fn round_digit(digits: &mut [u8], ndigits: usize, delta: u64, mut rem: u64, kappa: u64, frac: u64) {
    while rem < frac
        && delta.wrapping_sub(rem) >= kappa
        && (rem.wrapping_add(kappa) < frac
            || frac.wrapping_sub(rem) > rem.wrapping_add(kappa).wrapping_sub(frac))
    {
        digits[ndigits - 1] -= 1;
        rem = rem.wrapping_add(kappa);
    }
}

fn generate_digits(f: Fp, upper: Fp, lower: Fp, digits: &mut [u8; 18], k: &mut i32) -> usize {
    let wfrac = upper.frac.wrapping_sub(f.frac);
    let mut delta = upper.frac.wrapping_sub(lower.frac);
    let shift = u32::try_from(-upper.exp).unwrap_or(0);
    let one_frac = 1_u64 << shift;
    let mut part1 = upper.frac >> shift;
    let mut part2 = upper.frac & (one_frac - 1);
    let mut idx = 0;
    let mut kappa = 10;
    let mut divp = 10;
    while kappa > 0 {
        let div = TENS[divp];
        let digit = part1 / div;
        if digit != 0 || idx != 0 {
            digits[idx] = b'0' + u8::try_from(digit).unwrap_or(0);
            idx += 1;
        }
        part1 -= digit * div;
        kappa -= 1;
        let tmp = (part1 << shift).wrapping_add(part2);
        if tmp <= delta {
            *k += kappa;
            round_digit(digits, idx, delta, tmp, div << shift, wfrac);
            return idx;
        }
        divp += 1;
    }
    let mut unit = 18;
    loop {
        part2 = part2.wrapping_mul(10);
        delta = delta.wrapping_mul(10);
        kappa -= 1;
        let digit = part2 >> shift;
        if digit != 0 || idx != 0 {
            digits[idx] = b'0' + u8::try_from(digit).unwrap_or(0);
            idx += 1;
        }
        part2 &= one_frac - 1;
        if part2 < delta {
            *k += kappa;
            round_digit(
                digits,
                idx,
                delta,
                part2,
                one_frac,
                wfrac.wrapping_mul(TENS[unit]),
            );
            return idx;
        }
        unit -= 1;
    }
}

fn grisu2(d: f64, digits: &mut [u8; 18], k: &mut i32) -> usize {
    let mut w = build_fp(d);
    let (mut lower, mut upper) = normalized_boundaries(w);
    normalize(&mut w);
    let mut cached_k = 0;
    let cp = find_cachedpow10(upper.exp, &mut cached_k);
    w = multiply(w, cp);
    upper = multiply(upper, cp);
    lower = multiply(lower, cp);
    lower.frac = lower.frac.wrapping_add(1);
    upper.frac = upper.frac.wrapping_sub(1);
    *k = -cached_k;
    generate_digits(w, upper, lower, digits, k)
}

fn emit_digits(digits: &[u8], k: i32, neg: bool, out: &mut String) {
    let ndigits = i32::try_from(digits.len()).unwrap_or(0);
    let exp = (k + ndigits - 1).abs();
    let text = |d: &[u8]| String::from_utf8_lossy(d).into_owned();
    if k >= 0 && exp < 15 {
        out.push_str(&text(digits));
        out.extend(std::iter::repeat_n('0', usize::try_from(k).unwrap_or(0)));
        out.push_str(".0");
        return;
    }
    if k < 0 && (k > -7 || exp < 10) {
        let offset = ndigits - k.abs();
        if offset <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n(
                '0',
                usize::try_from(-offset).unwrap_or(0),
            ));
            out.push_str(&text(digits));
        } else {
            let (int, frac) = digits.split_at(usize::try_from(offset).unwrap_or(0));
            out.push_str(&text(int));
            out.push('.');
            out.push_str(&text(frac));
        }
        return;
    }
    let kept = ndigits.min(18 - i32::from(neg));
    let kept_digits = &digits[..usize::try_from(kept).unwrap_or(0)];
    out.push(char::from(kept_digits[0]));
    if kept > 1 {
        out.push('.');
        out.push_str(&text(&kept_digits[1..]));
    }
    out.push('e');
    out.push(if k + kept - 1 < 0 { '-' } else { '+' });
    let mut e = exp;
    let mut cent = 0;
    if e > 99 {
        cent = e / 100;
        out.push_str(&cent.to_string());
        e -= cent * 100;
    }
    if e > 9 {
        out.push_str(&(e / 10).to_string());
        e %= 10;
    } else if cent != 0 {
        out.push('0');
    }
    out.push_str(&(e % 10).to_string());
}

/// `fpconv_dtoa` for a finite double (json raises on NaN and Infinity before calling it).
pub fn dtoa(d: f64) -> String {
    let mut out = String::new();
    let neg = d.to_bits() & SIGNMASK != 0;
    if neg {
        out.push('-');
    }
    if d == 0.0 {
        out.push_str("0.0");
        return out;
    }
    let mut digits = [0_u8; 18];
    let mut k = 0;
    let n = grisu2(d, &mut digits, &mut k);
    emit_digits(&digits[..n], k, neg, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::dtoa;

    #[test]
    fn dtoa_cachedpow10_handles_extreme_subnormal_magnitudes() {
        let samples = [
            1.0e-308,
            2.2250738585072014e-308,
            5e-324,
            f64::MIN_POSITIVE / 2.0,
            f64::MIN_POSITIVE / 10.0,
        ];
        for value in samples {
            let text = dtoa(value);
            assert!(!text.is_empty(), "{value:e}");
        }
    }

    #[test]
    fn floats_print_as_ruby_json_prints_them() {
        let cases = [
            (0.1, "0.1"),
            (-0.0, "-0.0"),
            (100.0, "100.0"),
            (1e16, "1e+16"),
            (1.5e-7, "0.00000015"),
            (123_456_789_012_345_680.0, "1.2345678901234568e+17"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
        ];
        for (value, text) in cases {
            assert_eq!(dtoa(value), text, "{value:e}");
        }
    }
}
