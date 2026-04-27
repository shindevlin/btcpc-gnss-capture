// RTCM3 frame format: 0xD3 | 6-bit-zero + 10-bit-length | payload | 3-byte CRC-24Q

pub struct Frame {
    pub msg_type:     u16,
    pub payload_bytes: usize,
    pub raw:          Vec<u8>,
}

pub fn parse_frames(buf: &mut Vec<u8>) -> Vec<Frame> {
    let mut frames = Vec::new();
    loop {
        if buf.is_empty() { break; }
        if buf[0] != 0xD3 { buf.remove(0); continue; }
        if buf.len() < 3 { break; }

        let msg_len = (((buf[1] & 0x03) as usize) << 8) | buf[2] as usize;
        let frame_len = 3 + msg_len + 3;
        if buf.len() < frame_len { break; }

        let raw = buf[..frame_len].to_vec();

        let computed = crc24q(&raw[..3 + msg_len]);
        let stored = ((raw[frame_len - 3] as u32) << 16)
            | ((raw[frame_len - 2] as u32) << 8)
            | raw[frame_len - 1] as u32;

        if computed != stored {
            // Bad CRC — skip this byte and try to resync
            buf.remove(0);
            continue;
        }

        let msg_type = if msg_len >= 2 {
            ((raw[3] as u16) << 4) | (raw[4] as u16 >> 4)
        } else {
            0
        };

        frames.push(Frame { msg_type, payload_bytes: frame_len, raw });
        buf.drain(..frame_len);
    }
    frames
}

fn crc24q(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= (byte as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x1000000 != 0 {
                crc ^= 0x1864CFB;
            }
        }
    }
    crc & 0xFFFFFF
}
