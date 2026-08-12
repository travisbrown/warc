use nom::{
    IResult, Parser,
    bytes::streaming::{tag, take, take_while1},
    character::streaming::{line_ending, not_line_ending, space0, space1},
    combinator::complete,
    error::ErrorKind,
    multi::{many0, many1},
};
use std::borrow::Cow;
use std::str;

fn verify_error(input: &[u8]) -> nom::Err<nom::error::Error<&[u8]>> {
    nom::Err::Error(nom::error::Error::new(input, ErrorKind::Verify))
}

// TODO: evaluate the use of `ErrorKind::Verify` here.
fn version(input: &[u8]) -> IResult<&[u8], &str> {
    let (input, (_, version, _)) = (tag("WARC/"), not_line_ending, line_ending).parse(input)?;

    let version_str = str::from_utf8(version).map_err(|_| verify_error(input))?;

    Ok((input, version_str))
}

const fn is_header_token_char(chr: u8) -> bool {
    !matches!(chr, 0..=31
        | 128..=255
        | b'('
        | b')'
        | b'<'
        | b'>'
        | b'@'
        | b','
        | b';'
        | b':'
        | b'"'
        | b'/'
        | b'['
        | b']'
        | b'?'
        | b'='
        | b'{'
        | b'}'
        | b' '
        | b'\\')
}

/// Parse one named field, including any folded continuation lines.
///
/// The WARC grammar borrows the `LWS` rule from RFC 2616: a header line beginning with a space
/// or tab continues the previous field value, and each fold is read as a single space. Values
/// are borrowed unless folding forces a copy.
#[allow(clippy::type_complexity)]
fn header(input: &[u8]) -> IResult<&[u8], (&[u8], Cow<'_, [u8]>)> {
    let (input, (token, _, _, _, value, _)) = (
        take_while1(is_header_token_char),
        space0,
        tag(":"),
        space0,
        not_line_ending,
        line_ending,
    )
        .parse(input)?;

    // `complete` keeps a value ending exactly at the end of input from reporting `Incomplete`
    // while probing for a continuation line that is not there.
    let (input, continuations) =
        many0(complete((space1, not_line_ending, line_ending))).parse(input)?;

    let value = if continuations.is_empty() {
        Cow::Borrowed(value)
    } else {
        let mut folded = value.to_vec();
        for (_, continuation, _) in continuations {
            folded.push(b' ');
            folded.extend_from_slice(continuation);
        }
        Cow::Owned(folded)
    };

    Ok((input, (token, value)))
}

/// Parse a WARC header block.
// TODO: evaluate the use of `ErrorKind::Verify` here.
#[allow(clippy::type_complexity)]
pub fn headers(input: &[u8]) -> IResult<&[u8], (&str, Vec<(&str, Cow<'_, [u8]>)>, usize)> {
    let (input, version) = version(input)?;
    let (input, headers) = many1(header).parse(input)?;

    let mut content_length: Option<usize> = None;
    let mut warc_headers: Vec<(&str, Cow<'_, [u8]>)> = Vec::with_capacity(headers.len());

    for header in headers {
        let token_str = str::from_utf8(header.0).map_err(|_| verify_error(input))?;

        if content_length.is_none() && token_str.eq_ignore_ascii_case("content-length") {
            let value_str = str::from_utf8(&header.1).map_err(|_| verify_error(input))?;
            let len = value_str
                .parse::<usize>()
                .map_err(|_| verify_error(input))?;
            content_length = Some(len);
        }

        warc_headers.push((token_str, header.1));
    }

    // TODO: Technically if we didn't find a `content-length` header, the record is invalid. Should
    // we be returning an error here instead?
    Ok((input, (version, warc_headers, content_length.unwrap_or(0))))
}

/// Parse an entire WARC record.
#[allow(clippy::type_complexity)]
pub fn record(input: &[u8]) -> IResult<&[u8], (&str, Vec<(&str, Cow<'_, [u8]>)>, &[u8])> {
    let (input, (headers, _)) = (headers, line_ending).parse(input)?;
    let (input, (body, _, _)) = (take(headers.2), line_ending, line_ending).parse(input)?;

    Ok((input, (headers.0, headers.1, body)))
}

#[cfg(test)]
mod tests {
    use super::{header, headers, record, version};
    use nom::Err;
    use nom::Needed;
    use nom::error::ErrorKind;
    use std::borrow::Cow;

    #[test]
    fn version_parsing() {
        assert_eq!(version(&b"WARC/0.0\r\n"[..]), Ok((&b""[..], "0.0")));

        assert_eq!(version(&b"WARC/1.0\r\n"[..]), Ok((&b""[..], "1.0")));

        assert_eq!(
            version(&b"WARC/2.0-alpha\r\n"[..]),
            Ok((&b""[..], "2.0-alpha"))
        );
    }

    #[test]
    fn header_pair_parsing() {
        assert_eq!(
            header(&b"some-header: all/the/things\r\n"[..]),
            Ok((
                &b""[..],
                (&b"some-header"[..], Cow::Borrowed(&b"all/the/things"[..]))
            ))
        );

        assert_eq!(
            header(&b"another-header : with extra spaces\r\n"[..]),
            Ok((
                &b""[..],
                (
                    &b"another-header"[..],
                    Cow::Borrowed(&b"with extra spaces"[..])
                )
            ))
        );

        assert_eq!(
            header(&b"incomplete-header : missing-line-ending"[..]),
            Err(Err::Incomplete(Needed::Unknown))
        );
    }

    /// A field value may span lines via LWS continuation; each fold reads as a single space.
    #[test]
    fn header_pair_folded_value_parsing() {
        assert_eq!(
            header(&b"folded-header: line one\r\n line two\r\n\t \tline three\r\n"[..]),
            Ok((
                &b""[..],
                (
                    &b"folded-header"[..],
                    Cow::Owned(b"line one line two line three".to_vec())
                )
            ))
        );

        // A continuation line is part of the value, not the start of the next field.
        assert_eq!(
            header(&b"folded-header: one\r\n two\r\nnext-header: value\r\n"[..]),
            Ok((
                &b"next-header: value\r\n"[..],
                (&b"folded-header"[..], Cow::Owned(b"one two".to_vec()))
            ))
        );
    }

    #[test]
    fn headers_parsing() {
        let raw_invalid = b"\
            WARC/1.0\r\n\
            content-length: R2D2\r\n\
            that: is not\r\n\
            a-valid: content-length\r\n\
            \r\n\
        ";

        assert_eq!(
            headers(&raw_invalid[..]),
            Err(Err::Error(nom::error::Error::new(
                &b"\r\n"[..],
                ErrorKind::Verify
            )))
        );

        let raw = b"\
            WARC/1.0\r\n\
            content-length: 42\r\n\
            foo: is fantastic\r\n\
            bar: is beautiful\r\n\
            baz: is bananas\r\n\
            \r\n\
        ";
        let expected_version = "1.0";
        let expected_headers: Vec<(&str, Cow<'_, [u8]>)> = vec![
            ("content-length", Cow::Borrowed(b"42")),
            ("foo", Cow::Borrowed(b"is fantastic")),
            ("bar", Cow::Borrowed(b"is beautiful")),
            ("baz", Cow::Borrowed(b"is bananas")),
        ];
        let expected_len = 42;

        assert_eq!(
            headers(&raw[..]),
            Ok((
                &b"\r\n"[..],
                (expected_version, expected_headers, expected_len)
            ))
        );
    }

    #[test]
    fn parse_record() {
        let raw = b"\
            WARC/1.0\r\n\
            Warc-Type: dunno\r\n\
            Content-Length: 5\r\n\
            \r\n\
            12345\r\n\
            \r\n\
            WARC/1.0\r\n\
            Warc-Type: another\r\n\
            Content-Length: 6\r\n\
            \r\n\
            123456\r\n\
            \r\n\
        ";

        let expected_version = "1.0";
        let expected_headers: Vec<(&str, Cow<'_, [u8]>)> = vec![
            ("Warc-Type", Cow::Borrowed(b"dunno")),
            ("Content-Length", Cow::Borrowed(b"5")),
        ];
        let expected_body: &[u8] = b"12345";

        assert_eq!(
            record(&raw[..]),
            Ok((
                &b"WARC/1.0\r\nWarc-Type: another\r\nContent-Length: 6\r\n\r\n123456\r\n\r\n"[..],
                (expected_version, expected_headers, expected_body)
            ))
        );
    }
}
