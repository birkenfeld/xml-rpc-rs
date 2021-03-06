//! XML-RPC response parser.

use {Value, Response, Fault};
use error::ParseError;

use base64;
use xml::reader::{XmlEvent, EventReader};
use xml::name::OwnedName;
use xml::common::Position;
use xml::ParserConfig;
use iso8601::datetime;
use std::io::{self, ErrorKind, Read};
use std::collections::BTreeMap;

pub type ParseResult<T> = Result<T, ParseError>;

pub struct Parser<'a, R: Read + 'a> {
    reader: EventReader<&'a mut R>,
}

impl<'a, R: Read> Parser<'a, R> {
    pub fn new(reader: &'a mut R) -> Self {
        Parser {
            reader: EventReader::new_with_config(reader, ParserConfig {
                cdata_to_characters: true,
                ..Default::default()
            }),
        }
    }

    /// Reads an `XmlEvent` from a reader, disposing events we want to ignore.
    fn pull_event(&mut self) -> ParseResult<XmlEvent> {
        loop {
            let event = self.reader.next()?;
            match event {
                XmlEvent::StartDocument { .. }
                | XmlEvent::Comment(_)
                | XmlEvent::Whitespace(_)
                | XmlEvent::ProcessingInstruction { .. } => {},
                XmlEvent::StartElement { .. }
                | XmlEvent::EndElement { .. }
                | XmlEvent::EndDocument
                | XmlEvent::CData(_)
                | XmlEvent::Characters(_) => {
                    return Ok(event);
                }
            }
        }
    }

    /// Expects an opening tag like `<tag>` without attributes (and a local name without namespaces).
    fn expect_open(&mut self, tag: &str) -> ParseResult<()> {
        match self.pull_event()? {
            XmlEvent::StartElement { ref name, ref attributes, .. }
            if name == &OwnedName::local(tag) => {
                if !attributes.is_empty() {
                    return self.unexpected(format!("unexpected attributes in <{}>", tag));
                }

                Ok(())
            }
            _ => return self.unexpected(format!("expected <{}>", tag)),
        }
    }

    /// Expects a closing tag like `</tag>` with a local name without namespaces.
    fn expect_close(&mut self, tag: &str) -> ParseResult<()> {
        match self.pull_event()? {
            XmlEvent::EndElement { ref name } if name == &OwnedName::local(tag) => {
                Ok(())
            }
            _ => self.unexpected(format!("expected </{}>", tag)),
        }
    }

    /// Builds and returns an `Err(UnexpectedXml)`.
    fn unexpected<T, E: ToString>(&self, expected: E) -> ParseResult<T> {
        let expected = expected.to_string();
        let position = self.reader.position();

        Err(ParseError::UnexpectedXml {
            expected: expected,
            position: position,
        })
    }

    fn parse_response(&mut self) -> ParseResult<Response> {
        let response: Response;

        // <methodResponse>
        self.expect_open("methodResponse")?;

        // <fault> / <params>
        match self.pull_event()? {
            XmlEvent::StartElement { ref name, ref attributes, .. } => {
                if !attributes.is_empty() {
                    return self.unexpected("unexpected attributes");
                }

                if name == &OwnedName::local("fault") {
                    let value = self.parse_value()?;
                    let fault = Fault::from_value(&value).ok_or_else(|| {
                        io::Error::new(ErrorKind::Other, "malformed <fault>")
                    })?;
                    response = Err(fault);
                } else if name == &OwnedName::local("params") {
                    // <param>
                    self.expect_open("param")?;

                    let value = self.parse_value()?;
                    response = Ok(value);

                    // </param>
                    self.expect_close("param")?;
                } else {
                    return self.unexpected(format!("expected <fault> or <params>, got {}", name));
                }
            }
            _ => return self.unexpected("expected <fault> or <params>"),
        }

        Ok(response)
    }

    pub fn parse_value(&mut self) -> ParseResult<Value> {
        // <value>
        self.expect_open("value")?;

        let value = self.parse_value_inner()?;

        // </value>
        self.expect_close("value")?;

        Ok(value)
    }

    fn parse_value_inner(&mut self) -> ParseResult<Value> {
        let value: Value;

        // Raw string or specific type tag
        value = match self.pull_event()? {
            XmlEvent::StartElement { ref name, ref attributes, .. } => {
                if !attributes.is_empty() {
                    return self.unexpected(format!("unexpected attributes in <{}>", name));
                }

                if name == &OwnedName::local("struct") {
                    let mut members = BTreeMap::new();
                    loop {
                        match self.pull_event()? {
                            XmlEvent::EndElement { ref name } if name == &OwnedName::local("struct") => break,
                            XmlEvent::StartElement { ref name, ref attributes, .. } if name == &OwnedName::local("member") => {
                                // <member>
                                if !attributes.is_empty() {
                                    return self.unexpected(format!("unexpected attributes in <{}>", name));
                                }

                                // <name>NAME</name>
                                self.expect_open("name")?;
                                let name = match self.pull_event()? {
                                    XmlEvent::Characters(string) => string,
                                    _ => return self.unexpected("expected CDATA"),
                                };
                                self.expect_close("name")?;

                                // Value
                                let value = self.parse_value()?;

                                // </member>
                                self.expect_close("member")?;

                                members.insert(name, value);
                            }
                            _ => return self.unexpected("expected </struct> or <member>"),
                        }
                    }

                    Value::Struct(members)
                } else if name == &OwnedName::local("array") {
                    let mut elements: Vec<Value> = Vec::new();
                    self.expect_open("data")?;
                    loop {
                        match self.pull_event()? {
                            XmlEvent::EndElement { ref name } if name == &OwnedName::local("data") => break,
                            XmlEvent::StartElement { ref name, .. } if name == &OwnedName::local("value") => {
                                elements.push(self.parse_value_inner()?);
                                self.expect_close("value")?;
                            }
                            _event => return self.unexpected("expected </data> or <value>")
                        }
                    }
                    self.expect_close("array")?;
                    Value::Array(elements)
                } else if name == &OwnedName::local("nil") {
                    self.expect_close("nil")?;
                    Value::Nil
                } else if name == &OwnedName::local("string") {
                    let string = match self.pull_event()? {
                        XmlEvent::Characters(string) => {
                            self.expect_close(&name.local_name)?;
                            string
                        },
                        XmlEvent::EndElement { name: ref end_name } if end_name == name => String::new(),
                        _ => return self.unexpected("expected characters or </string>"),
                    };
                    Value::String(string)
                } else if name == &OwnedName::local("base64") {
                    let data = match self.pull_event()? {
                        XmlEvent::Characters(ref string) => base64::decode(string).map_err(|_| {
                            io::Error::new(ErrorKind::Other, format!("invalid value for base64: {}", string))
                        })?,
                        XmlEvent::EndElement { name: ref end_name } if end_name == name => Vec::new(),
                        _ => return self.unexpected("expected characters or </base64>"),
                    };
                    Value::Base64(data)
                } else {
                    // All other types expect raw characters...
                    let data = match self.pull_event()? {
                        XmlEvent::Characters(string) => string,
                        _ => return self.unexpected("expected characters"),
                    };

                    // ...and a corresponding close tag
                    self.expect_close(&name.local_name)?;

                    if name == &OwnedName::local("i4") || name == &OwnedName::local("int") {
                        Value::Int(data.parse::<i32>().map_err(|_| {
                            io::Error::new(ErrorKind::Other, format!("invalid value for integer: {}", data))
                        })?)
                    } else if name == &OwnedName::local("i8") {
                        Value::Int64(data.parse::<i64>().map_err(|_| {
                            io::Error::new(ErrorKind::Other, format!("invalid value for 64-bit integer: {}", data))
                        })?)
                    } else if name == &OwnedName::local("boolean") {
                        let val = match data.trim() {
                            "0" => false,
                            "1" => true,
                            _ => return Err(io::Error::new(ErrorKind::Other, format!("invalid value for <boolean>: {}", data)).into())
                        };

                        Value::Bool(val)
                    } else if name == &OwnedName::local("double") {
                        Value::Double(data.parse::<f64>().map_err(|_| {
                            io::Error::new(ErrorKind::Other, format!("invalid value for double: {}", data))
                        })?)
                    } else if name == &OwnedName::local("dateTime.iso8601") {
                        Value::DateTime(datetime(&data).map_err(|e| {
                            io::Error::new(ErrorKind::Other, format!("invalid value for dateTime.iso8601: {} ({})", data, e))
                        })?)
                    } else {
                        return self.unexpected("invalid <value> content");
                    }
                }
            }
            XmlEvent::Characters(string) => {
                Value::String(string)
            }
            _ => return self.unexpected("invalid <value> content"),
        };

        Ok(value)
    }
}

/// Parses a response from an XML reader.
pub fn parse_response<R: Read>(reader: &mut R) -> ParseResult<Response> {
    Parser::new(reader).parse_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use {Response, Value};
    use error::Fault;
    use std::fmt::Debug;

    fn read_response(xml: &str) -> ParseResult<Response> {
        parse_response(&mut xml.as_bytes())
    }

    fn read_value(xml: &str) -> ParseResult<Value> {
        Parser::new(&mut xml.as_bytes()).parse_value()
    }

    fn assert_ok<T: Debug, E: Debug>(result: Result<T, E>) {
        match result {
            Ok(t) => println!("assert_ok successful on Ok: {:?}", t),
            Err(e) => panic!("assert_ok called on Err value: {:?}", e),
        }
    }

    fn assert_err<T: Debug, E: Debug>(result: Result<T, E>) {
        match result {
            Ok(t) => panic!("assert_err called on Ok value: {:?}", t),
            Err(e) => println!("assert_err successful on Err: {:?}", e),
        }
    }

    #[test]
    fn parses_fault() {
        assert_eq!(read_response(r##"
<?xml version="1.0"?>
<methodResponse>
   <fault>
      <value>
         <struct>
            <member>
               <name>faultCode</name>
               <value><int>4</int></value>
               </member>
            <member>
               <name>faultString</name>
               <value><string>Too many parameters.</string></value>
               </member>
            </struct>
         </value>
      </fault>
   </methodResponse>"##),
        Ok(Err(Fault {
            fault_code: 4,
            fault_string: "Too many parameters.".into(),
        })));
    }

    #[test]
    fn ignores_additional_fault_fields() {
        assert_eq!(read_response(r##"
<?xml version="1.0"?>
<methodResponse>
   <fault>
      <value>
         <struct>
            <member>
               <name>faultCode</name>
               <value><int>4</int></value>
               </member>
            <member>
               <name>faultString</name>
               <value><string>Too many parameters.</string></value>
               </member>
            <member>
               <name>unnecessaryParameter</name>
               <value><string>Too many parameters.</string></value>
               </member>
            </struct>
         </value>
      </fault>
   </methodResponse>"##),
        Ok(Err(Fault {
            fault_code: 4,
            fault_string: "Too many parameters.".into(),
        })));
    }

    #[test]
    fn rejects_invalid_faults() {
        // Make sure to reject type errors in <fault>s - They're specified to contain specifically
        // typed fields.
        assert!(read_response(r##"
<?xml version="1.0"?>
<methodResponse>
   <fault>
      <value>
         <struct>
            <member>
               <name>faultCode</name>
               <value><string>I'm not an int!</string></value>
               </member>
            <member>
               <name>faultString</name>
               <value><string>Too many parameters.</string></value>
               </member>
            </struct>
         </value>
      </fault>
   </methodResponse>"##).is_err());
    }

    #[test]
    fn parses_string_value_with_whitespace() {
        assert_eq!(read_value("<value><string>  I'm a string!  </string></value>"),
            Ok(Value::String("  I'm a string!  ".into())));
    }

    #[test]
    fn parses_64bit_int() {
        assert_eq!(read_value("<value><i8>12345</i8></value>"),
            Ok(Value::Int64(12345)));
        assert_eq!(read_value("<value><i8>-100100100100</i8></value>"),
            Ok(Value::Int64(-100100100100)));
    }

    #[test]
    fn parses_int_with_plus_sign() {
        // "You can include a plus or minus at the beginning of a string of numeric characters."
        assert_eq!(read_value("<value><int>+1234</int></value>"),
            Ok(Value::Int(1234)));
    }

    #[test]
    fn parses_date_values() {
        assert_ok(read_value("<value><dateTime.iso8601>2015-02-18T23:16:09Z</dateTime.iso8601></value>"));
        assert_ok(read_value("<value><dateTime.iso8601>19980717T14:08:55</dateTime.iso8601></value>"));
        assert_err(read_value("<value><dateTime.iso8601></dateTime.iso8601></value>"));
        assert_err(read_value("<value><dateTime.iso8601>ILLEGAL VALUE :(</dateTime.iso8601></value>"));
    }

    #[test]
    fn parses_array_values() {
        assert_eq!(read_value(r#"
                <value><array><data>
                    <value><i4>5</i4></value>
                    <value><string>a</string></value>
                </data></array></value>"#),
            Ok(Value::Array(vec![Value::Int(5), Value::String("a".into())])));
    }

    #[test]
    fn parses_raw_value_as_string() {
        assert_eq!(read_value("<value>\t  I'm a string!  </value>"),
            Ok(Value::String("\t  I'm a string!  ".into())));
        // FIXME: Are empty <value>-tags also supposed to be parsed as empty strings?
    }

    #[test]
    fn parses_nil_values() {
        assert_eq!(read_value("<value><nil/></value>"), Ok(Value::Nil));
        assert_eq!(read_value("<value><nil></nil></value>"), Ok(Value::Nil));
        assert_err(read_value("<value><nil>ILLEGAL</nil></value>"));
    }

    #[test]
    fn unescapes_values() {
        assert_eq!(read_value("<value><string>abc&lt;abc&amp;abc</string></value>"),
            Ok(Value::String("abc<abc&abc".into())));
    }

    #[test]
    fn parses_empty_string() {
        assert_eq!(read_value("<value><string></string></value>"),
            Ok(Value::String(String::new())));
        assert_eq!(read_value("<value><string/></value>"),
            Ok(Value::String(String::new())));
    }

    #[test]
    fn parses_empty_base64() {
        assert_eq!(read_value("<value><base64></base64></value>"),
            Ok(Value::Base64(Vec::new())));
        assert_eq!(read_value("<value><base64/></value>"),
            Ok(Value::Base64(Vec::new())));
    }

    #[test]
    fn rejects_value_with_attributes() {
        // XXX we *should* reject everything with attributes (right?)
        assert_err(read_value(r#"<value name="ble">\t  I'm a string!  </value>"#));
    }
}
