use std::io::{Read, Write};
use std::path::Path;

#[cfg(feature = "native")]
use anyhow::Context;
use anyhow::{Result, anyhow, bail, ensure};
use bitcoin::XOnlyPublicKey;
use bitcoin::blockdata::constants::MAX_SCRIPT_ELEMENT_SIZE;
use bitcoin::opcodes;
use bitcoin::script::{self, PushBytes, ScriptBuf};
use brotli::enc::BrotliEncoderParams;
use brotli::enc::backward_references::BrotliEncoderMode;

pub const PROTOCOL_ID: [u8; 3] = *b"ord";
const BODY_TAG: [u8; 0] = [];

// Tag numbers and field order follow ord 0.29.0 (src/inscriptions/tag.rs and
// Inscription::append_reveal_script_to_builder), so ord indexes the envelope
// as a normal, uncursed inscription.
const TAG_CONTENT_TYPE: u8 = 1;
const TAG_METAPROTOCOL: u8 = 7;
const TAG_CONTENT_ENCODING: u8 = 9;

const BROTLI: &str = "br";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  Generic,
  Text,
  Font,
}

impl From<Mode> for BrotliEncoderMode {
  fn from(mode: Mode) -> Self {
    match mode {
      Mode::Generic => BrotliEncoderMode::BROTLI_MODE_GENERIC,
      Mode::Text => BrotliEncoderMode::BROTLI_MODE_TEXT,
      Mode::Font => BrotliEncoderMode::BROTLI_MODE_FONT,
    }
  }
}

// Copy of the ord 0.29.0 media table (src/inscriptions/media.rs). The first
// match on extension wins, as in ord.
const TABLE: &[(&str, Mode, &[&str])] = &[
  ("application/cbor", Mode::Generic, &["cbor"]),
  ("application/json", Mode::Text, &["json"]),
  ("application/octet-stream", Mode::Generic, &["bin"]),
  ("application/pdf", Mode::Generic, &["pdf"]),
  ("application/pgp-signature", Mode::Text, &["asc"]),
  ("application/protobuf", Mode::Generic, &["binpb"]),
  ("application/x-bittorrent", Mode::Generic, &["torrent"]),
  ("application/yaml", Mode::Text, &["yaml", "yml"]),
  ("audio/flac", Mode::Generic, &["flac"]),
  ("audio/mpeg", Mode::Generic, &["mp3"]),
  ("audio/ogg;codecs=opus", Mode::Generic, &["opus"]),
  ("audio/wav", Mode::Generic, &["wav"]),
  ("font/otf", Mode::Generic, &["otf"]),
  ("font/ttf", Mode::Generic, &["ttf"]),
  ("font/woff", Mode::Generic, &["woff"]),
  ("font/woff2", Mode::Font, &["woff2"]),
  ("image/apng", Mode::Generic, &["apng"]),
  ("image/avif", Mode::Generic, &["avif"]),
  ("image/gif", Mode::Generic, &["gif"]),
  ("image/jpeg", Mode::Generic, &["jpg", "jpeg"]),
  ("image/jxl", Mode::Generic, &["jxl"]),
  ("image/png", Mode::Generic, &["png"]),
  ("image/svg+xml", Mode::Text, &["svg"]),
  ("image/webp", Mode::Generic, &["webp"]),
  ("model/gltf+json", Mode::Text, &["gltf"]),
  ("model/gltf-binary", Mode::Generic, &["glb"]),
  ("model/stl", Mode::Generic, &["stl"]),
  ("text/css", Mode::Text, &["css"]),
  ("text/html;charset=utf-8", Mode::Text, &["html"]),
  ("text/javascript", Mode::Text, &["js", "mjs"]),
  ("text/markdown;charset=utf-8", Mode::Text, &["md"]),
  ("text/plain;charset=utf-8", Mode::Text, &["txt"]),
  ("text/x-python", Mode::Text, &["py"]),
  ("video/mp4", Mode::Generic, &["mp4"]),
  ("video/webm", Mode::Generic, &["webm"]),
];

/// The content type and brotli mode for a file name, from its extension.
pub fn content_type_for_name(name: &str) -> Result<(&'static str, Mode)> {
  let extension = Path::new(name)
    .extension()
    .and_then(|extension| extension.to_str())
    .ok_or_else(|| anyhow!("{name} has no file extension"))?
    .to_lowercase();

  TABLE
    .iter()
    .find(|(_, _, extensions)| extensions.contains(&extension.as_str()))
    .map(|(content_type, mode, _)| (*content_type, *mode))
    .ok_or_else(|| {
      anyhow!("{name}: the extension .{extension} has no known content type; set the content type")
    })
}

/// The brotli mode for a content type. Unknown types use the generic mode.
pub fn mode_for_content_type(content_type: &str) -> Mode {
  TABLE
    .iter()
    .find(|(known, _, _)| *known == content_type)
    .map(|(_, mode, _)| *mode)
    .unwrap_or(Mode::Generic)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inscription {
  pub content_type: Vec<u8>,
  pub content_encoding: Option<Vec<u8>>,
  pub metaprotocol: Option<Vec<u8>>,
  pub body: Vec<u8>,
}

impl Inscription {
  /// An inscription from a file name and its bytes. The content type comes
  /// from the extension when `content_type` is None.
  pub fn from_named_bytes(
    name: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
    compress: bool,
    metaprotocol: Option<&str>,
  ) -> Result<Self> {
    let (content_type, mode) = match content_type {
      Some(content_type) => (content_type, mode_for_content_type(content_type)),
      None => content_type_for_name(name)?,
    };

    Self::new(content_type, body, compress, mode, metaprotocol)
  }

  #[cfg(feature = "native")]
  pub fn from_file(
    path: &Path,
    content_type: Option<&str>,
    compress: bool,
    metaprotocol: Option<&str>,
  ) -> Result<Self> {
    let body = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Self::from_named_bytes(
      &path.to_string_lossy(),
      body,
      content_type,
      compress,
      metaprotocol,
    )
  }

  pub fn new(
    content_type: &str,
    body: Vec<u8>,
    compress: bool,
    mode: Mode,
    metaprotocol: Option<&str>,
  ) -> Result<Self> {
    ensure!(!content_type.is_empty(), "the content type is empty");

    let (body, content_encoding) = if compress {
      brotli_compress(mode, body)?
    } else {
      (body, None)
    };

    Ok(Self {
      content_type: content_type.as_bytes().to_vec(),
      content_encoding,
      metaprotocol: metaprotocol.map(|value| value.as_bytes().to_vec()),
      body,
    })
  }

  pub fn append_to(&self, mut builder: script::Builder) -> script::Builder {
    builder = builder
      .push_opcode(opcodes::OP_FALSE)
      .push_opcode(opcodes::all::OP_IF)
      .push_slice(PROTOCOL_ID);

    builder = append_tag(builder, TAG_CONTENT_TYPE, Some(&self.content_type));
    builder = append_tag(
      builder,
      TAG_CONTENT_ENCODING,
      self.content_encoding.as_deref(),
    );
    builder = append_tag(builder, TAG_METAPROTOCOL, self.metaprotocol.as_deref());

    builder = builder.push_slice(BODY_TAG);
    for chunk in self.body.chunks(MAX_SCRIPT_ELEMENT_SIZE) {
      builder = builder.push_slice(push_bytes(chunk));
    }

    builder.push_opcode(opcodes::all::OP_ENDIF)
  }

  /// The tapscript leaf of the commit output: a key-spend check followed by
  /// the inscription envelope.
  pub fn reveal_script(&self, key: &XOnlyPublicKey) -> ScriptBuf {
    let builder = script::Builder::new()
      .push_slice(key.serialize())
      .push_opcode(opcodes::all::OP_CHECKSIG);

    self.append_to(builder).into_script()
  }
}

fn append_tag(builder: script::Builder, tag: u8, value: Option<&[u8]>) -> script::Builder {
  match value {
    Some(value) => builder.push_slice([tag]).push_slice(push_bytes(value)),
    None => builder,
  }
}

fn push_bytes(data: &[u8]) -> &PushBytes {
  data
    .try_into()
    .expect("a script push is at most MAX_SCRIPT_ELEMENT_SIZE bytes")
}

// Same parameters as ord 0.29.0 (Inscription::compress). The compressed body
// replaces the original only when it is smaller.
fn brotli_compress(mode: Mode, data: Vec<u8>) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
  let mut compressor = brotli::CompressorWriter::with_params(
    Vec::new(),
    data.len(),
    &BrotliEncoderParams {
      lgblock: 24,
      lgwin: 24,
      mode: mode.into(),
      quality: 11,
      size_hint: data.len(),
      ..BrotliEncoderParams::default()
    },
  );
  compressor.write_all(&data)?;
  let compressed = compressor.into_inner();

  let mut decompressed = Vec::new();
  brotli::Decompressor::new(compressed.as_slice(), compressed.len())
    .read_to_end(&mut decompressed)?;
  if decompressed != data {
    bail!("the brotli round trip changed the content");
  }

  if compressed.len() < data.len() {
    Ok((compressed, Some(BROTLI.as_bytes().to_vec())))
  } else {
    Ok((data, None))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn content_types_follow_ord() {
    let cases = [
      ("a.html", "text/html;charset=utf-8"),
      ("a.PNG", "image/png"),
      ("a.opus", "audio/ogg;codecs=opus"),
      ("a.js", "text/javascript"),
      ("a.flac", "audio/flac"),
    ];
    for (path, expected) in cases {
      assert_eq!(content_type_for_name(path).unwrap().0, expected);
    }
    assert!(content_type_for_name("a.unknownext").is_err());
    assert!(content_type_for_name("noextension").is_err());
  }

  #[test]
  fn body_is_split_into_520_byte_pushes() {
    let inscription = Inscription::new(
      "application/octet-stream",
      vec![7; 1100],
      false,
      Mode::Generic,
      None,
    )
    .unwrap();
    let script = inscription.append_to(script::Builder::new()).into_script();
    let pushes: Vec<usize> = script
      .instructions()
      .filter_map(|instruction| match instruction.unwrap() {
        script::Instruction::PushBytes(bytes) => Some(bytes.len()),
        script::Instruction::Op(_) => None,
      })
      .collect();
    // OP_FALSE is an empty push. Then "ord", tag 1, content type, body tag,
    // and three body chunks.
    assert_eq!(pushes, vec![0, 3, 1, 24, 0, 520, 520, 60]);
  }

  #[test]
  fn compression_is_used_only_when_smaller() {
    let text = Inscription::new(
      "text/plain;charset=utf-8",
      vec![b'a'; 4000],
      true,
      Mode::Text,
      None,
    )
    .unwrap();
    assert_eq!(text.content_encoding.as_deref(), Some(b"br".as_slice()));
    assert!(text.body.len() < 4000);

    let random: Vec<u8> = (0..64u32)
      .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
      .collect();
    let binary = Inscription::new(
      "application/octet-stream",
      random.clone(),
      true,
      Mode::Generic,
      None,
    )
    .unwrap();
    assert_eq!(binary.content_encoding, None);
    assert_eq!(binary.body, random);
  }
}
