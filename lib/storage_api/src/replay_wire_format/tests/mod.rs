use super::ReplayRecord;

#[test]
pub fn can_decode_v1() {
    let encoded = include_bytes!("encoded_replay_v1.bin");
    let _replay_record = ReplayRecord::decode(encoded, 1);
}

#[test]
pub fn can_decode_v2() {
    let encoded = include_bytes!("encoded_replay_v2.bin");
    let _replay_record = ReplayRecord::decode(encoded, 2);
}

#[test]
pub fn can_decode_v3() {
    let encoded = include_bytes!("encoded_replay_v3.bin");
    let _replay_record = ReplayRecord::decode(encoded, 3);
}
