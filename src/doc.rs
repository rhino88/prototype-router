//! The response being put together for one request.
//!
//! Subgraph responses are kept as the bytes that arrived. Parsing one adds
//! nodes to a single vector; a node is a few integers saying where its key
//! and its value are in those bytes, and objects and arrays are linked lists
//! of nodes. Nothing is copied or allocated per value, no key is hashed, and
//! scalars reach the client as the text the subgraph sent.
//!
//! Nodes never move, so a node id stays good for the whole request, and
//! merging a response into the data is a matter of relinking nodes.

use axum::body::Bytes;

pub type Id = u32;
const NONE: Id = u32::MAX;
/// Deepest nesting a subgraph response may have.
const MAX_DEPTH: usize = 256;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Null,
    /// A string, number or boolean: `a..b` in the buffer is its JSON text.
    Raw,
    /// `a` is the first item, `b` the number of items.
    Arr,
    /// `a` is the first field, `b` the last.
    Obj,
}

#[derive(Clone, Copy)]
struct Node {
    kind: Kind,
    /// Buffer the key is in, and where: the text between the quotes.
    key_buf: u32,
    key_start: u32,
    key_end: u32,
    /// Buffer a `Raw` value is in.
    buf: u32,
    a: u32,
    b: u32,
    /// Next field of the object or item of the array this node is in.
    next: Id,
}

pub struct Doc {
    bufs: Vec<Bytes>,
    nodes: Vec<Node>,
}

/// The data starts as an empty object.
pub const ROOT: Id = 0;

impl Doc {
    /// With room for `nodes` nodes.
    pub fn new(nodes: usize) -> Doc {
        let root = Node { kind: Kind::Obj, key_buf: 0, key_start: 0, key_end: 0, buf: 0, a: NONE, b: NONE, next: NONE };
        let mut all = Vec::with_capacity(nodes.max(64));
        all.push(root);
        Doc { bufs: Vec::with_capacity(8), nodes: all }
    }

    /// Number of nodes there are.
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// Parses a JSON document and returns its node. With `unalias`, keys
    /// made by the planner's `alias_for` are read as the keys they stand for.
    pub fn parse(&mut self, bytes: Bytes, unalias: bool) -> Result<Id, String> {
        if std::str::from_utf8(&bytes).is_err() {
            return Err("response is not UTF-8".into());
        }
        if bytes.len() >= u32::MAX as usize {
            return Err("response is too large".into());
        }
        let buf = self.bufs.len() as u32;
        let mut parser = Parser { text: &bytes, at: 0, buf, unalias, nodes: &mut self.nodes };
        let parsed = parser.value(0).and_then(|id| {
            parser.space();
            if parser.at == parser.text.len() { Ok(id) } else { Err(()) }
        });
        let at = parser.at;
        self.bufs.push(bytes);
        parsed.map_err(|()| format!("invalid JSON at byte {}", at))
    }

    pub fn kind(&self, id: Id) -> Kind {
        self.nodes[id as usize].kind
    }

    pub fn key(&self, id: Id) -> &[u8] {
        let node = &self.nodes[id as usize];
        &self.bufs[node.key_buf as usize][node.key_start as usize..node.key_end as usize]
    }

    /// JSON text of a `Raw` node.
    pub fn raw(&self, id: Id) -> &[u8] {
        let node = &self.nodes[id as usize];
        &self.bufs[node.buf as usize][node.a as usize..node.b as usize]
    }

    /// The text of a string that needs no unescaping, such as a type name.
    pub fn str(&self, id: Id) -> Option<&str> {
        if self.kind(id) != Kind::Raw {
            return None;
        }
        let raw = self.raw(id);
        if raw.len() < 2 || raw[0] != b'"' || raw.contains(&b'\\') {
            return None;
        }
        std::str::from_utf8(&raw[1..raw.len() - 1]).ok()
    }

    /// Fields of an object or items of an array, in order.
    pub fn children(&self, id: Id) -> Children<'_> {
        let node = &self.nodes[id as usize];
        let next = match node.kind {
            Kind::Arr | Kind::Obj => node.a,
            _ => NONE,
        };
        Children { doc: self, next }
    }

    pub fn len(&self, array: Id) -> usize {
        match self.nodes[array as usize].kind {
            Kind::Arr => self.nodes[array as usize].b as usize,
            _ => 0,
        }
    }

    pub fn get(&self, object: Id, key: &str) -> Option<Id> {
        self.find(object, key.as_bytes())
    }

    /// The first field of an object, for `get_next`.
    pub fn start(&self, object: Id) -> Cursor {
        match self.nodes[object as usize].kind {
            Kind::Obj => Cursor(self.nodes[object as usize].a),
            _ => Cursor(NONE),
        }
    }

    /// `get` for reading fields one after another: the field after the one
    /// found last is tried first, which is the one wanted whenever the
    /// fields are read in the order they are in.
    pub fn get_next(&self, object: Id, cursor: &mut Cursor, key: &str) -> Option<Id> {
        let found = match cursor.0 {
            NONE => self.find(object, key.as_bytes())?,
            next if self.key(next) == key.as_bytes() => next,
            _ => self.find(object, key.as_bytes())?,
        };
        cursor.0 = self.nodes[found as usize].next;
        Some(found)
    }

    fn find(&self, object: Id, key: &[u8]) -> Option<Id> {
        let object = &self.nodes[object as usize];
        if object.kind != Kind::Obj {
            return None;
        }
        let mut id = object.a;
        while id != NONE {
            let field = &self.nodes[id as usize];
            // Most keys are told apart by their length, without reading them.
            if (field.key_end - field.key_start) as usize == key.len() && self.key(id) == key {
                return Some(id);
            }
            id = field.next;
        }
        None
    }

    fn append(&mut self, object: Id, field: Id) {
        self.nodes[field as usize].next = NONE;
        let last = self.nodes[object as usize].b;
        if last == NONE {
            self.nodes[object as usize].a = field;
        } else {
            self.nodes[last as usize].next = field;
        }
        self.nodes[object as usize].b = field;
    }

    /// A copy of `id` and everything under it, not linked to anything.
    fn copy(&mut self, id: Id) -> Id {
        let node = self.nodes[id as usize];
        let copy = self.nodes.len() as Id;
        self.nodes.push(Node { next: NONE, ..node });
        if matches!(node.kind, Kind::Arr | Kind::Obj) {
            let mut child = node.a;
            let mut last = NONE;
            while child != NONE {
                let next = self.nodes[child as usize].next;
                let child_copy = self.copy(child);
                if last == NONE {
                    self.nodes[copy as usize].a = child_copy;
                } else {
                    self.nodes[last as usize].next = child_copy;
                }
                last = child_copy;
                child = next;
            }
            if node.kind == Kind::Obj {
                self.nodes[copy as usize].b = last;
            }
        }
        copy
    }

    /// Gives `target` the value of `source`, keeping its key and its place.
    fn replace(&mut self, target: Id, source: Id, copy: bool) {
        let source = if copy { self.copy(source) } else { source };
        let from = self.nodes[source as usize];
        let node = &mut self.nodes[target as usize];
        node.kind = from.kind;
        node.buf = from.buf;
        node.a = from.a;
        node.b = from.b;
    }

    /// Merges `source` into `target`. `source` is used up unless `copy` is
    /// set, which leaves it as it was for another merge.
    ///
    /// `concrete` says whether a type name is an object type: an interface
    /// object's name never replaces the concrete type another subgraph has
    /// told us.
    pub fn merge(&mut self, target: Id, source: Id, copy: bool, concrete: &dyn Fn(&str) -> bool) {
        match (self.kind(target), self.kind(source)) {
            (Kind::Obj, Kind::Obj) => {
                let mut field = self.nodes[source as usize].a;
                while field != NONE {
                    let next = self.nodes[field as usize].next;
                    self.merge_field(target, field, copy, concrete);
                    field = next;
                }
            }
            (Kind::Arr, Kind::Arr) if self.len(target) == self.len(source) => {
                let mut existing = self.nodes[target as usize].a;
                let mut item = self.nodes[source as usize].a;
                while existing != NONE && item != NONE {
                    let next = self.nodes[item as usize].next;
                    self.merge(existing, item, copy, concrete);
                    existing = self.nodes[existing as usize].next;
                    item = next;
                }
            }
            // A subgraph that failed to resolve a field doesn't erase what
            // another one returned for it.
            (kind, Kind::Null) if kind != Kind::Null => {}
            _ => self.replace(target, source, copy),
        }
    }

    /// Merges one field of some other object into the object `target`.
    pub fn merge_field(&mut self, target: Id, field: Id, copy: bool, concrete: &dyn Fn(&str) -> bool) {
        match self.find(target, self.key(field)) {
            Some(existing) => {
                let keep = self.key(field) == b"__typename"
                    && self.str(existing).is_some()
                    && !self.str(field).is_some_and(concrete);
                if !keep {
                    self.merge(existing, field, copy, concrete);
                }
            }
            None => {
                let field = if copy { self.copy(field) } else { field };
                self.append(target, field);
            }
        }
    }

    /// Writes a value as JSON.
    pub fn write(&self, id: Id, out: &mut Vec<u8>) {
        match self.kind(id) {
            Kind::Null => out.extend_from_slice(b"null"),
            Kind::Raw => out.extend_from_slice(self.raw(id)),
            Kind::Arr => {
                out.push(b'[');
                for (index, item) in self.children(id).enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    self.write(item, out);
                }
                out.push(b']');
            }
            Kind::Obj => {
                out.push(b'{');
                for (index, field) in self.children(id).enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    out.push(b'"');
                    out.extend_from_slice(self.key(field));
                    out.extend_from_slice(b"\":");
                    self.write(field, out);
                }
                out.push(b'}');
            }
        }
    }

    pub fn to_string(&self, id: Id) -> String {
        let mut out = Vec::new();
        self.write(id, &mut out);
        // Buffers are checked to be UTF-8 and what is added between their
        // parts is ASCII.
        String::from_utf8(out).unwrap_or_default()
    }
}

/// See `Doc::get_next`.
pub struct Cursor(Id);

pub struct Children<'a> {
    doc: &'a Doc,
    next: Id,
}

impl Iterator for Children<'_> {
    type Item = Id;

    fn next(&mut self) -> Option<Id> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.doc.nodes[id as usize].next;
        Some(id)
    }
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    buf: u32,
    unalias: bool,
    nodes: &'a mut Vec<Node>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn space(&mut self) {
        while let Some(b' ' | b'\n' | b'\t' | b'\r') = self.peek() {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ()> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn push(&mut self, kind: Kind, a: u32, b: u32) -> Id {
        self.nodes.push(Node { kind, key_buf: self.buf, key_start: 0, key_end: 0, buf: self.buf, a, b, next: NONE });
        (self.nodes.len() - 1) as Id
    }

    /// Reads a string and returns where the text between its quotes is.
    fn string(&mut self) -> Result<(usize, usize), ()> {
        self.expect(b'"')?;
        let start = self.at;
        loop {
            // Eight bytes at a time up to the first quote, backslash or
            // control character.
            while let Some(bytes) = self.text.get(self.at..self.at + 8) {
                const ONES: u64 = 0x0101_0101_0101_0101;
                const HIGH: u64 = 0x8080_8080_8080_8080;
                let word = u64::from_le_bytes(bytes.try_into().unwrap());
                let zero = |word: u64| word.wrapping_sub(ONES) & !word & HIGH;
                let found = zero(word ^ (ONES * b'"' as u64))
                    | zero(word ^ (ONES * b'\\' as u64))
                    | (word.wrapping_sub(ONES * 0x20) & !word & HIGH);
                if found != 0 {
                    // The lowest mark is exact; the bytes below it are plain.
                    self.at += (found.trailing_zeros() / 8) as usize;
                    break;
                }
                self.at += 8;
            }
            match self.peek().ok_or(())? {
                b'"' => {
                    self.at += 1;
                    return Ok((start, self.at - 1));
                }
                b'\\' => {
                    self.at += 1;
                    match self.peek().ok_or(())? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.at += 1,
                        b'u' => {
                            let digits = self.text.get(self.at + 1..self.at + 5).ok_or(())?;
                            if !digits.iter().all(u8::is_ascii_hexdigit) {
                                return Err(());
                            }
                            self.at += 5;
                        }
                        _ => return Err(()),
                    }
                }
                0..=0x1f => return Err(()),
                _ => self.at += 1,
            }
        }
    }

    fn digits(&mut self) -> Result<(), ()> {
        let start = self.at;
        while let Some(b'0'..=b'9') = self.peek() {
            self.at += 1;
        }
        if self.at == start { Err(()) } else { Ok(()) }
    }

    fn number(&mut self) -> Result<(), ()> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        if self.peek() == Some(b'0') {
            self.at += 1;
        } else {
            self.digits()?;
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            self.digits()?;
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.at += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.at += 1;
            }
            self.digits()?;
        }
        Ok(())
    }

    fn literal(&mut self, word: &[u8], kind: Kind) -> Result<Id, ()> {
        if !self.text[self.at..].starts_with(word) {
            return Err(());
        }
        let start = self.at;
        self.at += word.len();
        Ok(self.push(kind, start as u32, self.at as u32))
    }

    fn value(&mut self, depth: usize) -> Result<Id, ()> {
        if depth > MAX_DEPTH {
            return Err(());
        }
        self.space();
        match self.peek().ok_or(())? {
            b'{' => {
                self.at += 1;
                let object = self.push(Kind::Obj, NONE, NONE);
                let mut last = NONE;
                self.space();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(object);
                }
                loop {
                    self.space();
                    let (mut start, end) = self.string()?;
                    if self.unalias {
                        // Checked to be UTF-8 as a whole, and a key ends at a quote.
                        let key = std::str::from_utf8(&self.text[start..end]).map_err(|_| ())?;
                        start = end - crate::plan::unalias(key).len();
                    }
                    self.space();
                    self.expect(b':')?;
                    let field = self.value(depth + 1)?;
                    let node = &mut self.nodes[field as usize];
                    node.key_start = start as u32;
                    node.key_end = end as u32;
                    if last == NONE {
                        self.nodes[object as usize].a = field;
                    } else {
                        self.nodes[last as usize].next = field;
                    }
                    last = field;
                    self.space();
                    match self.peek().ok_or(())? {
                        b',' => self.at += 1,
                        b'}' => {
                            self.at += 1;
                            self.nodes[object as usize].b = last;
                            return Ok(object);
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'[' => {
                self.at += 1;
                let array = self.push(Kind::Arr, NONE, 0);
                let mut last = NONE;
                let mut count = 0;
                self.space();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(array);
                }
                loop {
                    let item = self.value(depth + 1)?;
                    if last == NONE {
                        self.nodes[array as usize].a = item;
                    } else {
                        self.nodes[last as usize].next = item;
                    }
                    last = item;
                    count += 1;
                    self.space();
                    match self.peek().ok_or(())? {
                        b',' => self.at += 1,
                        b']' => {
                            self.at += 1;
                            self.nodes[array as usize].b = count;
                            return Ok(array);
                        }
                        _ => return Err(()),
                    }
                }
            }
            b'"' => {
                let (start, end) = self.string()?;
                Ok(self.push(Kind::Raw, start as u32 - 1, end as u32 + 1))
            }
            b't' => self.literal(b"true", Kind::Raw),
            b'f' => self.literal(b"false", Kind::Raw),
            b'n' => self.literal(b"null", Kind::Null),
            _ => {
                let start = self.at;
                self.number()?;
                Ok(self.push(Kind::Raw, start as u32, self.at as u32))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> (Doc, Id) {
        let mut doc = Doc::new(0);
        let id = doc.parse(Bytes::copy_from_slice(text.as_bytes()), false).unwrap();
        (doc, id)
    }

    fn merged(target: &str, source: &str, copy: bool) -> String {
        let (mut doc, target) = parsed(target);
        let source = doc.parse(Bytes::copy_from_slice(source.as_bytes()), false).unwrap();
        doc.merge(target, source, copy, &|name| name == "Book");
        if copy {
            // The source is still whole.
            doc.merge(ROOT, source, false, &|_| true);
        }
        doc.to_string(target)
    }

    #[test]
    fn writes_what_it_read() {
        let text = r#"{"a":[1,-2.5e+3,true,false,null,"x\"y\u00e9\n"],"b":{},"c":[],"d":{"e":"é"}}"#;
        let (doc, id) = parsed(text);
        assert_eq!(doc.to_string(id), text);
        let (doc, id) = parsed(" { \"a\" : [ 1 , 2 ] }\n");
        assert_eq!(doc.to_string(id), r#"{"a":[1,2]}"#);
    }

    #[test]
    fn reads_long_strings() {
        // Every special byte at every offset of the eight read at once.
        for special in ["\\\"", "\\\\", "\\u00e9", "é", "\\n"] {
            for offset in 0..20 {
                let text = format!("[\"{}{}tail\",\"{}\"]", "x".repeat(offset), special, "y".repeat(offset));
                let (doc, id) = parsed(&text);
                assert_eq!(doc.to_string(id), text);
            }
        }
        for offset in 0..20 {
            for bad in ["\n", "\u{1f}", "\\x"] {
                let text = format!("\"{}{}tail\"", "x".repeat(offset), bad);
                let mut doc = Doc::new(0);
                assert!(doc.parse(Bytes::from(text.clone()), false).is_err(), "{:?}", text);
            }
            let text = format!("\"{}", "x".repeat(offset));
            assert!(Doc::new(0).parse(Bytes::from(text), false).is_err());
        }
    }

    #[test]
    fn reads_fields_in_turn() {
        let (doc, id) = parsed(r#"{"a":1,"b":2,"c":3}"#);
        let mut cursor = doc.start(id);
        for key in ["a", "b", "c", "a", "c", "x", "b", "c"] {
            assert_eq!(doc.get_next(id, &mut cursor, key), doc.get(id, key), "{}", key);
        }
        let (doc, id) = parsed("[1]");
        assert_eq!(doc.get_next(id, &mut doc.start(id), "a"), None);
    }

    #[test]
    fn rejects_invalid_json() {
        for text in [
            "", "{", "[1,]", "{\"a\":1,}", "{a:1}", "01", "1.", "-", "1e", "\"a", "\"\\x\"", "\"\\u12g4\"",
            "\"a\nb\"", "tru", "nul", "{} {}", "[1 2]", "{\"a\" 1}",
        ] {
            let mut doc = Doc::new(0);
            assert!(doc.parse(Bytes::copy_from_slice(text.as_bytes()), false).is_err(), "{:?}", text);
        }
        let mut doc = Doc::new(0);
        assert!(doc.parse(Bytes::from_static(b"\"\xff\""), false).is_err());
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(doc.parse(Bytes::from(deep), false).is_err());
    }

    #[test]
    fn reads_aliased_keys() {
        let mut doc = Doc::new(0);
        let id = doc.parse(Bytes::from_static(br#"{"_a4_Bookid":1,"_k_id":2}"#), true).unwrap();
        assert_eq!(doc.to_string(id), r#"{"id":1,"_k_id":2}"#);
    }

    #[test]
    fn looks_up() {
        let (doc, id) = parsed(r#"{"__typename":"Book","n":1,"s":"a\\b","list":[1,2,3]}"#);
        assert_eq!(doc.get(id, "__typename").and_then(|t| doc.str(t)), Some("Book"));
        assert_eq!(doc.get(id, "n").and_then(|t| doc.str(t)), None);
        assert_eq!(doc.get(id, "s").and_then(|t| doc.str(t)), None);
        assert_eq!(doc.get(id, "missing"), None);
        assert_eq!(doc.len(doc.get(id, "list").unwrap()), 3);
    }

    #[test]
    fn merges() {
        for copy in [false, true] {
            // New fields are added, objects and lists of one length are merged.
            assert_eq!(
                merged(r#"{"a":1,"o":{"x":1},"l":[{"x":1},{"x":2}]}"#, r#"{"b":2,"o":{"y":2},"l":[{"y":1},{"y":2}]}"#, copy),
                r#"{"a":1,"o":{"x":1,"y":2},"l":[{"x":1,"y":1},{"x":2,"y":2}],"b":2}"#
            );
            // A null doesn't erase a value; anything else replaces it.
            assert_eq!(merged(r#"{"a":1,"b":null,"c":[1]}"#, r#"{"a":null,"b":2,"c":[1,2]}"#, copy), r#"{"a":1,"b":2,"c":[1,2]}"#);
            // Only an object type's name replaces a type name.
            assert_eq!(merged(r#"{"__typename":"Book"}"#, r#"{"__typename":"Media"}"#, copy), r#"{"__typename":"Book"}"#);
            assert_eq!(merged(r#"{"__typename":"Media"}"#, r#"{"__typename":"Book"}"#, copy), r#"{"__typename":"Book"}"#);
        }
    }

    #[test]
    fn copies_leave_the_source_alone() {
        let (mut doc, first) = parsed(r#"{"a":{"x":1}}"#);
        let second = doc.parse(Bytes::from_static(br#"{"a":{"x":2}}"#), false).unwrap();
        let entity = doc.parse(Bytes::from_static(br#"{"a":{"y":[1,{"z":2}]},"b":"s"}"#), false).unwrap();
        doc.merge(first, entity, true, &|_| true);
        doc.merge(second, entity, false, &|_| true);
        let more = doc.parse(Bytes::from_static(br#"{"a":{"w":0}}"#), false).unwrap();
        doc.merge(first, more, false, &|_| true);
        assert_eq!(doc.to_string(first), r#"{"a":{"x":1,"y":[1,{"z":2}],"w":0},"b":"s"}"#);
        assert_eq!(doc.to_string(second), r#"{"a":{"x":2,"y":[1,{"z":2}]},"b":"s"}"#);
    }
}
