#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rlp {
    Bytes(Vec<u8>),
    List(Vec<Rlp>),
}

impl Rlp {
    pub fn is_bytes(&self) -> bool {
        matches!(self, Rlp::Bytes(_))
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let Rlp::Bytes(bytes) = self {
            Some(bytes)
        } else {
            None
        }
    }

    pub fn as_list(&self) -> Option<&[Rlp]> {
        if let Rlp::List(list) = self {
            Some(list)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub enum RlpError {
    InputTooShort,
    InvalidPrefix,
}

/// Decode one RLP item starting at `i`. Returns (item, new_index).
fn decode_item(input: &[u8], i: usize) -> Result<(Rlp, usize), RlpError> {
    if i >= input.len() {
        return Err(RlpError::InputTooShort);
    }
    let prefix = input[i];

    // Case 1: single byte < 0x80 → itself
    if prefix <= 0x7f {
        return Ok((Rlp::Bytes(vec![prefix]), i + 1));
    }

    // Case 2: string with length < 55
    if prefix <= 0xb7 {
        let len = (prefix - 0x80) as usize;
        let start = i + 1;
        let end = start + len;
        if end > input.len() {
            return Err(RlpError::InputTooShort);
        }
        return Ok((Rlp::Bytes(input[start..end].to_vec()), end));
    }

    // Case 3: string with length > 55
    if prefix <= 0xbf {
        let len_of_len = (prefix - 0xb7) as usize;
        let start = i + 1;
        let end_len = start + len_of_len;
        if end_len > input.len() {
            return Err(RlpError::InputTooShort);
        }
        let len = decode_be(&input[start..end_len]);
        let end = end_len + len;
        if end > input.len() {
            return Err(RlpError::InputTooShort);
        }
        return Ok((Rlp::Bytes(input[end_len..end].to_vec()), end));
    }

    // Case 4: list with total payload <= 55
    if prefix <= 0xf7 {
        let len = (prefix - 0xc0) as usize;
        let mut items = Vec::new();
        let mut idx = i + 1;
        let end = idx + len;
        while idx < end {
            let (item, new_idx) = decode_item(input, idx)?;
            items.push(item);
            idx = new_idx;
        }
        return Ok((Rlp::List(items), end));
    }

    // Case 5: list with total payload > 55
    let len_of_len = (prefix - 0xf7) as usize;
    let start = i + 1;
    let end_len = start + len_of_len;
    if end_len > input.len() {
        return Err(RlpError::InputTooShort);
    }
    let len = decode_be(&input[start..end_len]);
    let mut items = Vec::new();
    let mut idx = end_len;
    let end = idx + len;
    while idx < end {
        let (item, new_idx) = decode_item(input, idx)?;
        items.push(item);
        idx = new_idx;
    }
    return Ok((Rlp::List(items), end));
}

/// helper: decode big-endian bytes into usize
fn decode_be(bytes: &[u8]) -> usize {
    let mut out = 0usize;
    for &b in bytes {
        out = (out << 8) | (b as usize);
    }
    out
}

/// Public entry: decode a whole buffer
pub fn decode(input: &[u8]) -> Result<Rlp, RlpError> {
    let (item, idx) = decode_item(input, 0)?;
    if idx != input.len() {
        // trailing bytes
        return Err(RlpError::InvalidPrefix);
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_single_byte() {
        let input = [0x7f];
        let r = decode(&input).unwrap();
        assert_eq!(r, Rlp::Bytes(vec![0x7f]));
    }

    #[test]
    fn decode_short_string() {
        let input = [0x83, b'd', b'o', b'g'];
        assert_eq!(decode(&input).unwrap(), Rlp::Bytes(b"dog".to_vec()));
    }

    #[test]
    fn decode_list_of_strings() {
        // RLP for ["cat", "dog"] = 0xc8 0x83 'c' 'a' 't' 0x83 'd' 'o' 'g'
        let input = [0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g'];
        assert_eq!(
            decode(&input).unwrap(),
            Rlp::List(vec![
                Rlp::Bytes(b"cat".to_vec()),
                Rlp::Bytes(b"dog".to_vec())
            ])
        );
    }

    #[test]
    fn decode_trailing_bytes_error() {
        // 0x80 = empty string (consumes 1 byte), extra trailing byte -> error
        let input = [0x80, 0x00];
        assert!(matches!(decode(&input), Err(RlpError::InvalidPrefix)));
    }

    #[test]
    fn decode_input_too_short() {
        // 0x83 indicates 3-byte string but only 2 bytes provided
        let input = [0x83, b'd', b'o'];
        assert!(matches!(decode(&input), Err(RlpError::InputTooShort)));
    }

    #[test]
    fn complex_test() {
        println!("hello");
        let data = hex::decode("f90221f901fba00000000000000000000000000000000000000000000000000000000000000000a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347940000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000a0dee01c5533553bd0d455007cfb545d1aee8f3c1c4f185f7a5e5190d2517898c9a00000000000000000000000000000000000000000000000000000000000000000b901000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080088405f5e1008301981b84689f38ab80a000000000000000000000000000000000000000000000000000000000000000018800000000000000008203e8e1a028786ebadf71284e6e93c14139e79ce80b2f8b85acc25d6d9664fba042795e76c0").unwrap();
        let rlp = decode(&data).unwrap();
        let elems = rlp.as_list().unwrap();

        println!("elems length: {}", elems.len());

        let header = &elems[0].as_list().unwrap();
        let transactions = &elems[1].as_list().unwrap();
        // elems [2] is ommers, and elems[3] is (optional) withdrawals
        println!("Transactions len: {}", transactions.len());
        for tx in transactions.iter() {
            let tx = hex::encode(tx.as_bytes().unwrap());
            println!("Transaction: {}", tx);
        }

        println!("Header: {:?}", header);
        println!("Header length: {}", header.len());

        for (i, val) in header.iter().enumerate() {
            if let Rlp::Bytes(bytes) = val {
                println!("Header[{}]: bytes: {}", i, hex::encode(bytes));
            } else {
                println!("Header[{}]: {:?}", i, val);
            }
        }

        // 0 - should be parent (but all 0s??)
        // 1 - should be ommers - some const value?
        // 2 - should be beneficiary
        // 3- state root (zeros?)
        // 4 - transction root (set)
        // 5 - receipts root (empty) - zeros
        // 6 - bloom (empty) - zeros (lots)
        // 7 - difficulty ('really' empty)
        // 8 - block number (matches)
        // 9 - gas limit
        // 10 - gas used
        // 11 - timestamp
        // 12 - extra data (really empty)
        // 13 - mix hash (set to '1')
        // 14 - nonce (set to '00000')
        // 15 - base fee per gas? set to 03e8

        assert!(false);
    }
}
