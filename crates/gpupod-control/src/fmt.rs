//! 数値の表示整形（Django の floatformat / unlocalize 相当）。

/// Python の float 表現（repr と同一の最短往復表記、整数値は ".0" 付き）。
pub fn py_float(value: f64) -> String {
    format!("{value:?}")
}

/// bytes / 1024^3（Python 版 resources.GIB との float 除算と同じ結果）。
pub fn gib(bytes: u64) -> f64 {
    bytes as f64 / gpupod_common::resources::GIB as f64
}

/// Django の floatformat:N（N 桁、ROUND_HALF_UP、小数は必ず N 桁）。
pub fn floatformat(value: f64, decimals: usize) -> String {
    round_decimal(&py_float(value), decimals)
}

/// bytes を GiB 表示（floatformat:N）。
pub fn gib_label(bytes: u64, decimals: usize) -> String {
    floatformat(gib(bytes), decimals)
}

/// 10 進文字列を小数 N 桁へ ROUND_HALF_UP で丸める（Django の Decimal.quantize 相当）。
pub fn round_decimal(text: &str, decimals: usize) -> String {
    let (sign, body) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((head, tail)) => (head, tail),
        None => (body, ""),
    };
    if int_part.is_empty()
        || !int_part.bytes().all(|byte| byte.is_ascii_digit())
        || !frac_part.bytes().all(|byte| byte.is_ascii_digit())
    {
        // inf / nan 等は Python 版と同じくそのまま返す。
        return text.to_string();
    }
    if frac_part.len() <= decimals {
        let mut fraction = frac_part.to_string();
        while fraction.len() < decimals {
            fraction.push('0');
        }
        return if decimals == 0 {
            format!("{sign}{int_part}")
        } else {
            format!("{sign}{int_part}.{fraction}")
        };
    }

    let mut fraction: Vec<u8> = frac_part.as_bytes()[..decimals]
        .iter()
        .map(|b| b - b'0')
        .collect();
    let next_digit = frac_part.as_bytes()[decimals] - b'0';
    if next_digit >= 5 {
        let mut carry = 1u16;
        for index in (0..fraction.len()).rev() {
            let value = fraction[index] as u16 + carry;
            if value >= 10 {
                fraction[index] = 0;
                carry = 1;
            } else {
                fraction[index] = value as u8;
                carry = 0;
                break;
            }
        }
        if carry > 0 {
            let mut digits: Vec<u8> = int_part.bytes().map(|b| b - b'0').collect();
            let mut position = digits.len();
            while carry > 0 {
                if position == 0 {
                    digits.insert(0, 1);
                    carry = 0;
                    break;
                }
                position -= 1;
                let value = digits[position] as u16 + carry;
                if value >= 10 {
                    digits[position] = 0;
                    carry = 1;
                } else {
                    digits[position] = value as u8;
                    carry = 0;
                }
            }
            let rebuilt: String = digits.iter().map(|d| (b'0' + d) as char).collect();
            let fraction_text: String = fraction.iter().map(|d| (b'0' + d) as char).collect();
            return if decimals == 0 {
                format!("{sign}{rebuilt}")
            } else {
                format!("{sign}{rebuilt}.{fraction_text}")
            };
        }
    }
    let fraction_text: String = fraction.iter().map(|d| (b'0' + d) as char).collect();
    if decimals == 0 {
        format!("{sign}{int_part}")
    } else {
        format!("{sign}{int_part}.{fraction_text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floatformat_keeps_fixed_decimals() {
        assert_eq!(floatformat(8.0, 2), "8.00");
        assert_eq!(floatformat(0.125, 2), "0.13");
        assert_eq!(floatformat(0.1, 2), "0.10");
        assert_eq!(floatformat(1234.5678, 1), "1234.6");
        assert_eq!(floatformat(1234.55, 1), "1234.6");
        assert_eq!(floatformat(9.99, 1), "10.0");
        assert_eq!(floatformat(-1.005, 2), "-1.01");
    }

    #[test]
    fn python_float_repr() {
        assert_eq!(py_float(8.0), "8.0");
        assert_eq!(py_float(0.0625), "0.0625");
        assert_eq!(gib_label(6 * 1024 * 1024 * 1024, 2), "6.00");
        assert_eq!(py_float(gib(6 * 1024 * 1024 * 1024)), "6.0");
    }
}
