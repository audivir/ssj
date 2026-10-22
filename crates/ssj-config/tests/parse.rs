use ssj_config::parse;

#[derive(Debug, confique::Config)]
#[config(layer_attr(serde(deny_unknown_fields)))]
struct Sample {
    #[config(default = "x")]
    name: String,
    #[config(default = 5)]
    count: u32,
}

#[test]
fn errors_name_their_line_and_a_bad_value_names_its_key() {
    for (text, expected) in [
        ("\n\ncount = x", "line 3:"),
        ("count = \"five\"", "line 1: count: "),
        ("\nname = 1\n", "line 2: name: "),
        ("\nnmae = \"y\"", "line 2: unknown field `nmae`"),
        ("name = \"a\"\nname = \"b\"", "line 2:"),
        ("[section]", "line 1:"),
    ] {
        let error = parse::<Sample>(text).unwrap_err();
        assert!(error.starts_with(expected), "{text:?}: {error}");
    }
}

#[test]
fn keys_that_the_file_leaves_out_get_their_defaults() {
    let sample = parse::<Sample>("# only a comment\ncount = 7 # and another\n").unwrap();
    assert_eq!((sample.name.as_str(), sample.count), ("x", 7));
}
