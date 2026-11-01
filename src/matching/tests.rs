use super::*;

fn document(sets: &str, count: usize, size: usize, cursor: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><eExact xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:noNamespaceSchemaLocation=\"eExact-XML.xsd\"><MatchSets>{sets}</MatchSets><Topics><Topic code=\"MatchSets\" ts_d=\"{cursor}\" count=\"{count}\" pagesize=\"{size}\"/></Topics><Messages/></eExact>"
    )
}

fn set(lines: &str) -> String {
    format!(
        "<MatchSet><GLAccount code=\" 01200 \"/><Account code=\" 00042 \"/><MatchLines>{lines}</MatchLines></MatchSet>"
    )
}

fn line(entry: i64, amount: &str) -> String {
    format!(
        "<MatchLine finyear=\"2025\" finperiod=\"12\" journal=\" 70 \" entry=\"{entry}\" amountdc=\"{amount}\"/>"
    )
}

#[test]
fn parses_precise_signed_multi_member_allocations_and_retains_duplicate_groups() {
    let lines = format!(
        "{}{}{}",
        line(1, "0.1234567890123456789012345678"),
        line(2, "-0.1"),
        line(3, "-0.0234567890123456789012345678")
    );
    let set = set(&lines);
    let xml = document(&format!("{set}{set}"), 2, 1000, "0x0000000000000002");
    let parsed = parse_export(xml.as_bytes()).unwrap();
    assert_eq!(parsed.sets.sets.len(), 2);
    let first = &parsed.sets.sets[0];
    assert_eq!(first.gl_account.code, "01200");
    assert_eq!(first.account.code, "00042");
    assert_eq!(first.lines.lines[0].journal, "70");
    assert_eq!(
        serde_json::to_value(Money::from_xml(&first.lines.lines[0].amount).unwrap()).unwrap(),
        "0.1234567890123456789012345678"
    );
}

#[test]
fn rejects_incomplete_malformed_unsafe_or_inconsistent_exports_without_financial_text() {
    let base = document(
        &set(&format!("{}{}", line(1, "12"), line(2, "-12"))),
        1,
        1000,
        "0x0000000000000002",
    );
    let cases = vec![
        String::new(), base[..base.len()-5].into(), format!("{base}{base}"),
        base.replace("<eExact ","<wrong "),
        base.replace("<MatchSets>","<MatchSets><Unexpected/>"),
        base.replace("<Messages/>","<Messages><Message><Description>PRIVATE_FINANCIAL_TEXT</Description></Message></Messages>"),
        base.replace("<eExact ","<!DOCTYPE eExact [<!ENTITY leak SYSTEM 'file:///tmp/private'>]><eExact "),
        base.replace("<MatchSets>","<MatchSets><?unsafe leak?>"),
        base.replace("<Messages/>","<Messages><![CDATA[PRIVATE_FINANCIAL_TEXT]]></Messages>"),
        base.replace("count=\"1\"","count=\"2\""),
        base.replace("pagesize=\"1000\"","pagesize=\"0\""),
        base.replace("pagesize=\"1000\"","pagesize=\"1001\""),
        base.replace("ts_d=\"0x0000000000000002\"","ts_d=\"https://attacker.invalid\""),
        base.replace("code=\"MatchSets\"","code=\"GLTransactions\""),
        base.replace("amountdc=\"-12\"","amountdc=\"-11\""),
        base.replace("amountdc=\"-12\"","amountdc=\"PRIVATE_FINANCIAL_TEXT\""),
        base.replace("finyear=\"2025\"","finyear=\"0\""),
        base.replace("finperiod=\"12\"","finperiod=\"-1\""),
        base.replace("entry=\"1\"","entry=\"0\""),
        base.replace("journal=\" 70 \"","journal=\" \""),
        base.replace("code=\" 01200 \"","code=\" \""),
        base.replace("code=\" 00042 \"","code=\" \""),
        base.replace("amountdc=\"-12\"","amountdc=\"-12\" amountdc=\"-12\""),
        base.replace("code=\" 00042 \"","code=\" &unknown; \""),
        base.replace("<Topics>","<Topics><Topic code=\"MatchSets\" ts_d=\"0x0000000000000002\" count=\"1\" pagesize=\"1000\"/>"),
    ];
    for xml in cases {
        let error = match parse_export(xml.as_bytes()) {
            Ok(_) => panic!("Accepted bad XML: {xml}"),
            Err(error) => error,
        };
        assert!(!format!("{error:#}").contains("PRIVATE_FINANCIAL_TEXT"));
    }
}

#[test]
fn empty_export_and_cursor_validation_are_explicit() {
    assert!(
        parse_export(document("", 0, 1000, "0x0000000000000000").as_bytes())
            .unwrap()
            .sets
            .sets
            .is_empty()
    );
    for valid in ["0x0000000000000000", "0x0123456789aBcDeF"] {
        assert!(valid_cursor(valid));
    }
    for invalid in [
        "",
        "0x1",
        "0X0123456789ABCDEF",
        "0x0123456789ABCDEG",
        "0x0123456789ABCDEF&Topic=Accounts",
    ] {
        assert!(!valid_cursor(invalid));
    }
}

#[test]
fn terminal_empty_envelope_is_supported_but_nonempty_sets_require_metadata() {
    assert!(
        parse_export(b"<eExact><Messages/></eExact>")
            .unwrap()
            .sets
            .sets
            .is_empty()
    );
    let xml = format!(
        "<eExact><MatchSets>{}</MatchSets><Messages/></eExact>",
        set(&format!("{}{}", line(1, "12"), line(2, "-12")))
    );
    assert!(parse_export(xml.as_bytes()).is_err());
    assert!(parse_export(b"<eExact/>").is_err());
}

#[test]
fn xml_decimal_attributes_reject_rounding_non_decimal_syntax_and_overflow() {
    for bad in [
        "1_000",
        "1e3",
        "NaN",
        " 12",
        "12 ",
        "++12",
        "1.2.3",
        ".",
        "79228162514264337593543950336",
        "0.12345678901234567890123456789",
    ] {
        assert!(Money::from_xml(bad).is_err(), "Accepted {bad}");
    }
    let overflow = set(&format!(
        "{}{}{}{}",
        line(1, "79228162514264337593543950335"),
        line(2, "1"),
        line(3, "-79228162514264337593543950335"),
        line(4, "-1")
    ));
    assert!(parse_export(document(&overflow, 1, 1000, "0x0000000000000001").as_bytes()).is_err());
}
