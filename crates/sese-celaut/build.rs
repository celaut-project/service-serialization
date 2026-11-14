//! Compile protos/celaut.proto (and buffer.proto) to a descriptor set, with no protoc.
use prost::Message;

fn main() {
    println!("cargo:rerun-if-changed=protos");
    let set = protox::compile(["celaut.proto"], ["protos"]).expect("compile protos/celaut.proto");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("celaut.bin"), set.encode_to_vec()).expect("write the descriptor set");
}
