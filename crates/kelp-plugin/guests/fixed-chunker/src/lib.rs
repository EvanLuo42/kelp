//! Splits files into chunks of 1000 bytes.

wit_bindgen::generate!({ path: "../../wit", world: "chunker" });

struct Plugin;

impl Guest for Plugin {
    fn chunk(_path: String, data: Vec<u8>) -> Vec<Span> {
        (0..data.len())
            .step_by(1000)
            .map(|offset| Span {
                offset: offset as u64,
                len: (data.len() - offset).min(1000) as u32,
            })
            .collect()
    }
}

export!(Plugin);
