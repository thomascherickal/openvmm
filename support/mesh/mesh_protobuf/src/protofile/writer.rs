// Copyright (C) Microsoft Corporation. All rights reserved.

//! Code to write .proto files from descriptors.

use super::FieldDescriptor;
use super::FieldType;
use super::MessageDescriptor;
use super::OneofDescriptor;
use super::TopLevelDescriptor;
use crate::protofile::FieldKind;
use crate::protofile::MessageDescription;
use crate::protofile::SequenceType;
use heck::ToUpperCamelCase;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// A type used to write protobuf descriptors to `.proto`-format files.
pub struct DescriptorWriter<'a> {
    descriptors: Vec<&'a TopLevelDescriptor<'a>>,
    file_heading: &'a str,
}

impl<'a> DescriptorWriter<'a> {
    /// Returns a new object for writing the `.proto` files described by
    /// `descriptors`.
    ///
    /// `descriptors` only needs to contain the roots of the protobuf
    /// message graph; any other message types referred to by the types in
    /// `descriptors` will be found and written to `.proto` files as well.
    pub fn new(descriptors: impl IntoIterator<Item = &'a MessageDescription<'a>>) -> Self {
        // First find all the descriptors starting with the provided roots.
        let mut descriptors = referenced_descriptors(descriptors);

        // Sort the descriptors to get a consistent order from run to run and build to build.
        descriptors.sort_by_key(|desc| (desc.package, desc.message.name));
        // Deduplicate by package and name. TODO: ensure duplicates match.
        descriptors.dedup_by_key(|desc| (desc.package, desc.message.name));

        Self {
            descriptors,
            file_heading: "",
        }
    }

    /// Sets the file heading written to each file.
    pub fn file_heading(&mut self, file_heading: &'a str) -> &mut Self {
        self.file_heading = file_heading;
        self
    }

    /// Writes the `.proto` files to writers returned by `f`.
    pub fn write<W: Write>(&self, mut f: impl FnMut(&str) -> io::Result<W>) -> io::Result<()> {
        let mut descriptors = self.descriptors.iter().copied().peekable();
        while let Some(&first) = descriptors.peek() {
            let file = f(&package_proto_file(first.package))?;
            let mut writer = PackageWriter::new(first.package, Box::new(file));
            write!(
                writer,
                "{file_heading}// Autogenerated, do not edit.\n\nsyntax = \"proto3\";\npackage {proto_package};\n",
                file_heading = self.file_heading,
                proto_package = first.package,
            )?;
            writer.nl_next();

            // Collect imports.
            let mut imports = Vec::new();
            let n = {
                let mut descriptors = descriptors.clone();
                let mut n = 0;
                while descriptors
                    .peek()
                    .map_or(false, |d| d.package == first.package)
                {
                    let desc = descriptors.next().unwrap();
                    desc.message.collect_imports(&mut writer, &mut imports)?;
                    n += 1;
                }
                n
            };

            imports.sort();
            imports.dedup();
            for import in imports {
                writeln!(writer, "import \"{import}\";")?;
            }

            writer.nl_next();

            // Collect messages.
            for desc in (&mut descriptors).take(n) {
                desc.message.fmt(&mut writer)?;
            }
        }
        Ok(())
    }

    /// Writes the `.proto` files to disk, rooted at `path`.
    ///
    /// Returns the paths of written files.
    pub fn write_to_path(&self, path: impl AsRef<Path>) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        self.write(|name| {
            let path = path.as_ref().join(name);
            if let Some(parent) = path.parent() {
                fs_err::create_dir_all(parent)?;
            }
            let file = fs_err::File::create(&path)?;
            paths.push(path);
            Ok(file)
        })?;
        Ok(paths)
    }
}

struct PackageWriter<'a, 'w> {
    writer: Box<dyn 'w + Write>,
    needs_nl: bool,
    needs_indent: bool,
    indent: String,
    package: &'a str,
}

impl<'a, 'w> PackageWriter<'a, 'w> {
    fn new(package: &'a str, writer: Box<dyn 'w + Write>) -> Self {
        Self {
            writer,
            needs_nl: false,
            needs_indent: false,
            indent: String::new(),
            package,
        }
    }

    fn indent(&mut self) {
        self.indent += "  ";
    }

    fn unindent(&mut self) {
        self.indent.truncate(self.indent.len() - 2);
        self.needs_nl = false;
    }

    fn nl_next(&mut self) {
        self.needs_nl = true;
    }
}

impl Write for PackageWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.first() == Some(&b'\n') {
            self.writer.write_all(b"\n")?;
            self.needs_nl = false;
            self.needs_indent = true;
            return Ok(1);
        }
        if self.needs_nl {
            self.writer.write_all(b"\n")?;
            self.needs_nl = false;
        }
        if self.needs_indent {
            self.writer.write_all(self.indent.as_bytes())?;
            self.needs_indent = false;
        }
        self.writer.write_all(buf)?;
        if buf.last() == Some(&b'\n') {
            self.needs_indent = true;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Computes the referenced descriptors from a set of descriptors.
fn referenced_descriptors<'a>(
    descriptors: impl IntoIterator<Item = &'a MessageDescription<'a>>,
) -> Vec<&'a TopLevelDescriptor<'a>> {
    let mut descriptors =
        Vec::from_iter(descriptors.into_iter().copied().filter_map(|d| match d {
            MessageDescription::Internal(tld) => Some(tld),
            MessageDescription::External { .. } => None,
        }));
    let mut inserted = HashSet::from_iter(descriptors.iter().copied());

    fn process_field_type<'a>(
        field_type: &FieldType<'a>,
        descriptors: &mut Vec<&'a TopLevelDescriptor<'a>>,
        inserted: &mut HashSet<&'a TopLevelDescriptor<'a>>,
    ) {
        match field_type.kind {
            FieldKind::Message(tld) => {
                if let MessageDescription::Internal(tld) = tld() {
                    if inserted.insert(tld) {
                        descriptors.push(tld);
                    }
                }
            }
            FieldKind::Tuple(tys) => {
                for ty in tys {
                    process_field_type(ty, descriptors, inserted);
                }
            }
            FieldKind::KeyValue(tys) => {
                for ty in tys {
                    process_field_type(ty, descriptors, inserted);
                }
            }
            FieldKind::Builtin(_) | FieldKind::Local(_) | FieldKind::External { .. } => {}
        }
    }

    fn process_message<'a>(
        message: &MessageDescriptor<'a>,
        descriptors: &mut Vec<&'a TopLevelDescriptor<'a>>,
        inserted: &mut HashSet<&'a TopLevelDescriptor<'a>>,
    ) {
        for field in message
            .fields
            .iter()
            .chain(message.oneofs.iter().flat_map(|oneof| oneof.variants))
        {
            process_field_type(&field.field_type, descriptors, inserted);
        }
        for inner in message.messages {
            process_message(inner, descriptors, inserted);
        }
    }

    let mut i = 0;
    while let Some(&tld) = descriptors.get(i) {
        process_message(tld.message, &mut descriptors, &mut inserted);
        i += 1;
    }

    descriptors
}

fn package_proto_file(package: &str) -> String {
    format!("{}.proto", package)
}

impl<'a> MessageDescriptor<'a> {
    fn collect_imports(
        &self,
        w: &mut PackageWriter<'a, '_>,
        imports: &mut Vec<Cow<'a, str>>,
    ) -> io::Result<()> {
        for message in self.messages {
            message.collect_imports(w, imports)?;
        }
        for oneof in self.oneofs {
            for field in oneof.variants {
                field.field_type.collect_imports(w, imports)?;
            }
        }
        for field in self.fields {
            field.field_type.collect_imports(w, imports)?;
        }
        Ok(())
    }

    fn fmt(&self, w: &mut PackageWriter<'_, '_>) -> io::Result<()> {
        if !self.comment.is_empty() {
            for line in self.comment.split('\n') {
                writeln!(w, "//{line}")?;
            }
        }
        writeln!(w, "message {} {{", self.name)?;
        w.indent();
        for message in self.messages {
            message.fmt(w)?;
        }
        for oneof in self.oneofs {
            oneof.fmt_nested_messages(w)?;
        }
        for field in self.fields {
            field.fmt_nested_message(w)?;
        }
        for oneof in self.oneofs {
            oneof.fmt(w)?;
        }
        for field in self.fields {
            field.fmt(w)?;
        }
        w.unindent();
        writeln!(w, "}}")?;
        w.nl_next();
        Ok(())
    }
}

impl<'a> FieldType<'a> {
    fn collect_imports(
        &self,
        w: &mut PackageWriter<'a, '_>,
        imports: &mut Vec<Cow<'a, str>>,
    ) -> io::Result<()> {
        match self.kind {
            FieldKind::Builtin(_) | FieldKind::Local(_) => {}
            FieldKind::External { import_path, .. } => {
                imports.push(import_path.into());
            }
            FieldKind::Message(f) => match f() {
                MessageDescription::Internal(tld) => {
                    if w.package != tld.package {
                        imports.push(package_proto_file(tld.package).into());
                    }
                }
                MessageDescription::External {
                    name: _,
                    import_path,
                } => {
                    imports.push(import_path.into());
                }
            },
            FieldKind::Tuple(field_types) => {
                for field_type in field_types {
                    field_type.collect_imports(w, imports)?;
                }
            }
            FieldKind::KeyValue(field_types) => {
                for field_type in field_types {
                    field_type.collect_imports(w, imports)?;
                }
            }
        }
        Ok(())
    }
}

impl FieldDescriptor<'_> {
    fn fmt_nested_message(&self, w: &mut PackageWriter<'_, '_>) -> io::Result<()> {
        match self.field_type.kind {
            FieldKind::Tuple(field_types) => {
                self.fmt_tuple_message(
                    w,
                    field_types,
                    (1..=field_types.len()).map(|i| format!("field{i}")),
                )?;
            }
            FieldKind::KeyValue(field_types) => {
                self.fmt_tuple_message(w, field_types, ["key", "value"])?;
            }
            FieldKind::Builtin(_)
            | FieldKind::Local(_)
            | FieldKind::External { .. }
            | FieldKind::Message(_) => {}
        }
        Ok(())
    }

    fn fmt_tuple_message(
        &self,
        w: &mut PackageWriter<'_, '_>,
        field_types: &[FieldType<'_>],
        names: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<(), io::Error> {
        let fields = field_types
            .iter()
            .enumerate()
            .zip(names)
            .map(|((i, field_type), name)| (field_type, i as u32 + 1, name))
            .collect::<Vec<_>>();
        let fields = fields
            .iter()
            .map(|(&ty, number, name)| FieldDescriptor::new("", ty, name.as_ref(), *number))
            .collect::<Vec<_>>();
        MessageDescriptor::new(&self.name.to_upper_camel_case(), "", &fields, &[], &[]).fmt(w)?;
        Ok(())
    }

    fn fmt(&self, w: &mut PackageWriter<'_, '_>) -> io::Result<()> {
        if !self.comment.is_empty() {
            for line in self.comment.split('\n') {
                writeln!(w, "//{}", line.trim_end())?;
            }
        }

        let is_message = match self.field_type.kind {
            FieldKind::Builtin(_) => false,
            FieldKind::Local(_)
            | FieldKind::External { .. }
            | FieldKind::Message(_)
            | FieldKind::Tuple(_)
            | FieldKind::KeyValue { .. } => true,
        };

        match self.field_type.sequence_type {
            // Message fields are implicitly optional.
            Some(SequenceType::Optional) if !is_message => write!(w, "optional ")?,
            None | Some(SequenceType::Optional) => {}
            Some(SequenceType::Repeated) => write!(w, "repeated ")?,
            Some(SequenceType::Map(key)) => write!(w, "map<{key}, ")?,
        };
        match self.field_type.kind {
            FieldKind::Builtin(name) | FieldKind::Local(name) => write!(w, "{}", name)?,
            FieldKind::External { name, .. } => write!(w, ".{}", name)?,
            FieldKind::Message(tld) => match tld() {
                MessageDescription::Internal(tld) => {
                    write!(w, ".{}.{}", tld.package, tld.message.name)?;
                }
                MessageDescription::External {
                    name,
                    import_path: _,
                } => {
                    write!(w, ".{name}")?;
                }
            },
            FieldKind::Tuple(_) | FieldKind::KeyValue(_) => {
                write!(w, "{}", self.name.to_upper_camel_case())?
            }
        }
        if matches!(self.field_type.sequence_type, Some(SequenceType::Map(_))) {
            write!(w, ">")?;
        }
        write!(w, " {} = {};", self.name, self.field_number)?;
        if !self.field_type.annotation.is_empty() {
            write!(w, " // {}", self.field_type.annotation)?;
        }
        writeln!(w)
    }
}

impl OneofDescriptor<'_> {
    fn fmt_nested_messages(&self, w: &mut PackageWriter<'_, '_>) -> io::Result<()> {
        for variant in self.variants {
            if variant.field_type.is_sequence() {
                FieldDescriptor {
                    field_type: FieldType::tuple(&[variant.field_type]),
                    ..*variant
                }
                .fmt_nested_message(w)?;
            } else {
                variant.fmt_nested_message(w)?;
            }
        }
        Ok(())
    }

    fn fmt(&self, w: &mut PackageWriter<'_, '_>) -> io::Result<()> {
        writeln!(w, "oneof {} {{", self.name)?;
        w.indent();
        for variant in self.variants {
            if variant.field_type.is_sequence() {
                FieldDescriptor {
                    field_type: FieldType::tuple(&[variant.field_type]),
                    ..*variant
                }
                .fmt(w)?;
            } else {
                variant.fmt(w)?;
            }
        }
        w.unindent();
        writeln!(w, "}}")?;
        w.nl_next();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::DescriptorWriter;
    use crate::protofile::message_description;
    use crate::Protobuf;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::Write;

    /// Comment on this guy.
    #[derive(Protobuf)]
    #[mesh(package = "test")]
    struct Foo {
        /// Doc comment
        #[mesh(1)]
        x: u32,
        #[mesh(2)]
        t: (u32,),
        #[mesh(3)]
        t2: (),
        #[mesh(4)]
        bar: (u32, ()),
        /// Another doc comment
        /// (multi-line)
        #[mesh(5)]
        y: Vec<u32>,
        /**
        multi
        line
        */
        #[mesh(6)]
        b: (),
        #[mesh(7)]
        repeated_self: Vec<Foo>,
        #[mesh(8)]
        e: Bar,
        #[mesh(9)]
        nested_repeat: Vec<Vec<u32>>,
        #[mesh(10)]
        proto_map: HashMap<String, (u32,)>,
        #[mesh(11)]
        vec_map: HashMap<u32, Vec<u32>>,
        #[mesh(12)]
        bad_array: [u32; 3],
        #[mesh(13)]
        wrapped_array: [String; 3],
    }

    #[derive(Protobuf)]
    #[mesh(package = "test")]
    enum Bar {
        #[mesh(1)]
        This,
        #[mesh(2)]
        This2(),
        #[mesh(3, transparent)]
        That(u32),
        #[mesh(4)]
        Other {
            #[mesh(1)]
            hi: bool,
            #[mesh(2)]
            hello: u32,
        },
        #[mesh(5, transparent)]
        Repeat(Vec<u32>),
        #[mesh(6, transparent)]
        DoubleRepeat(Vec<Vec<u32>>),
    }

    struct BorrowedWriter<T>(RefCell<T>);

    impl<T: Write> Write for &BorrowedWriter<T> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.borrow_mut().flush()
        }
    }

    #[test]
    fn test() {
        let writer = BorrowedWriter(RefCell::new(Vec::<u8>::new()));
        DescriptorWriter::new(&[message_description::<Foo>()])
            .write(|_name| Ok(&writer))
            .unwrap();
        let s = String::from_utf8(writer.0.into_inner()).unwrap();
        let expected = r#"// Autogenerated, do not edit.

syntax = "proto3";
package test;

import "google/protobuf/empty.proto";
import "google/protobuf/wrappers.proto";

message Bar {
  message Other {
    bool hi = 1;
    uint32 hello = 2;
  }

  message Repeat {
    repeated uint32 field1 = 1;
  }

  message DoubleRepeat {
    message Field1 {
      repeated uint32 field1 = 1;
    }

    repeated Field1 field1 = 1;
  }

  oneof variant {
    .google.protobuf.Empty this = 1;
    .google.protobuf.Empty this2 = 2;
    uint32 that = 3;
    Other other = 4;
    Repeat repeat = 5;
    DoubleRepeat double_repeat = 6;
  }
}

// Comment on this guy.
message Foo {
  message Bar {
    uint32 field1 = 1;
    .google.protobuf.Empty field2 = 2;
  }

  message NestedRepeat {
    repeated uint32 field1 = 1;
  }

  message VecMap {
    uint32 key = 1;
    repeated uint32 value = 2;
  }

  message WrappedArray {
    repeated string field1 = 1;
  }

  // Doc comment
  uint32 x = 1;
  .google.protobuf.UInt32Value t = 2;
  .google.protobuf.Empty t2 = 3;
  Bar bar = 4;
  // Another doc comment
  // (multi-line)
  repeated uint32 y = 5;
  //
  //        multi
  //        line
  //
  .google.protobuf.Empty b = 6;
  repeated .test.Foo repeated_self = 7;
  .test.Bar e = 8;
  repeated NestedRepeat nested_repeat = 9;
  map<string, .google.protobuf.UInt32Value> proto_map = 10;
  repeated VecMap vec_map = 11;
  repeated uint32 bad_array = 12; // packed repr only
  WrappedArray wrapped_array = 13;
}
"#;
        if s != expected {
            for diff in diff::lines(expected, &s) {
                match diff {
                    diff::Result::Left(l) => println!("-{}", l),
                    diff::Result::Both(l, _) => println!(" {}", l),
                    diff::Result::Right(r) => println!("+{}", r),
                }
            }
            panic!();
        }
    }
}