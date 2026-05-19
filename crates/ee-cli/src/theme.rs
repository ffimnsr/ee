use ratatui::style::Color;

pub(crate) mod ui {
    use super::Color;

    pub(crate) const BG_APP: Color = Color::Rgb(22, 24, 31);
    pub(crate) const BG_CHROME: Color = Color::Rgb(30, 32, 39);
    pub(crate) const BG_CHROME_ALT: Color = Color::Rgb(24, 25, 38);
    pub(crate) const BG_STATUS: Color = Color::Rgb(49, 54, 68);
    pub(crate) const BG_CURSOR_LINE: Color = Color::Rgb(35, 38, 50);
    pub(crate) const BG_SELECTION: Color = Color::Rgb(68, 71, 90);
    pub(crate) const BG_COLOR_COLUMN: Color = Color::Rgb(55, 35, 35);
    pub(crate) const BG_FIND: Color = Color::Rgb(250, 179, 135);
    pub(crate) const BG_ANNOTATION: Color = Color::Rgb(43, 82, 74);
    pub(crate) const BG_SWIFT_LABEL: Color = Color::Rgb(245, 194, 231);
    pub(crate) const BG_OVERLAY_SHADOW: Color = Color::Rgb(8, 10, 15);
    pub(crate) const BG_PICKER: Color = Color::Rgb(16, 18, 24);
    pub(crate) const BG_PICKER_QUERY: Color = Color::Rgb(19, 22, 30);
    pub(crate) const BG_PICKER_RESULTS: Color = Color::Rgb(14, 16, 22);
    pub(crate) const BG_PICKER_ROW_ALT: Color = Color::Rgb(18, 20, 28);

    pub(crate) const BORDER_MUTED: Color = Color::Rgb(88, 91, 112);
    pub(crate) const BORDER_PICKER: Color = Color::Rgb(94, 196, 214);
    pub(crate) const BORDER_PICKER_QUERY: Color = Color::Rgb(59, 66, 86);
    pub(crate) const BORDER_PICKER_RESULTS: Color = Color::Rgb(48, 54, 72);

    pub(crate) const FG_MUTED: Color = Color::Rgb(166, 173, 200);
    pub(crate) const FG_TEXT: Color = Color::Rgb(205, 214, 244);
    pub(crate) const FG_DIM: Color = Color::Rgb(148, 156, 187);
    pub(crate) const FG_KEY: Color = Color::Rgb(137, 220, 235);
    pub(crate) const FG_TAB_INACTIVE: Color = Color::Rgb(186, 194, 222);
    pub(crate) const FG_BUFFER: Color = Color::Rgb(213, 216, 224);
    pub(crate) const FG_STATUS_FILE: Color = Color::Rgb(238, 238, 238);
    pub(crate) const FG_STATUS_FLAG: Color = Color::Rgb(100, 120, 150);
    pub(crate) const FG_SUCCESS: Color = Color::Rgb(166, 227, 161);
    pub(crate) const FG_WARNING: Color = Color::Rgb(250, 179, 135);
    pub(crate) const FG_ERROR: Color = Color::Rgb(243, 139, 168);
    pub(crate) const FG_INFO: Color = Color::Rgb(137, 180, 250);
    pub(crate) const FG_MARKER_HINT: Color = Color::Rgb(166, 227, 161);
    pub(crate) const FG_INVERTED: Color = Color::Rgb(11, 14, 20);
    pub(crate) const FG_SUBTLE: Color = Color::Rgb(70, 80, 100);
    pub(crate) const FG_TILDE: Color = Color::Rgb(65, 72, 95);
    pub(crate) const FG_FOLD: Color = Color::Rgb(100, 130, 160);
    pub(crate) const FG_GUTTER_DIM: Color = Color::Rgb(90, 100, 125);
    pub(crate) const FG_LOADING: Color = Color::Rgb(90, 95, 115);
    pub(crate) const FG_EMPTY: Color = Color::DarkGray;
    pub(crate) const FG_PICKER_TITLE: Color = Color::Rgb(232, 236, 241);
    pub(crate) const FG_PICKER_SUBTLE: Color = Color::Rgb(116, 126, 147);
    pub(crate) const FG_PICKER_COUNT: Color = Color::Rgb(158, 167, 188);
    pub(crate) const FG_PICKER_PLACEHOLDER: Color = Color::Rgb(94, 104, 126);
    pub(crate) const FG_PICKER_QUERY: Color = Color::Rgb(224, 228, 235);
    pub(crate) const FG_PICKER_EMPTY: Color = Color::Rgb(112, 121, 144);
    pub(crate) const FG_PICKER_INDEX: Color = Color::Rgb(92, 102, 124);
    pub(crate) const FG_PICKER_FOOTER: Color = Color::Rgb(121, 130, 151);
}

pub(crate) mod syntax {
    use super::Color;

    pub(crate) const FG_COMMENT: Color = Color::Rgb(101, 115, 126);
    pub(crate) const FG_STRING: Color = Color::Rgb(195, 151, 66);
    pub(crate) const FG_NUMBER: Color = Color::Rgb(211, 120, 70);
    pub(crate) const FG_KEYWORD: Color = Color::Rgb(180, 142, 173);
    pub(crate) const FG_FUNCTION: Color = Color::Rgb(136, 192, 208);
    pub(crate) const FG_TYPE: Color = Color::Rgb(143, 188, 187);
    pub(crate) const FG_VARIABLE: Color = Color::Rgb(216, 222, 233);
    pub(crate) const FG_TAG_OPERATOR: Color = Color::Rgb(129, 161, 193);
    pub(crate) const FG_PUNCTUATION: Color = Color::Rgb(171, 178, 191);
    pub(crate) const FG_INVALID: Color = Color::Rgb(239, 83, 80);
    pub(crate) const FG_HEADING: Color = Color::Rgb(94, 129, 172);
}
