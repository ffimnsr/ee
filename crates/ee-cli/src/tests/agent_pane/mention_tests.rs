//! `impl App` agents-pane tests: `@` mention picker domain.
use super::*;

fn mention_fixture(temp: &tempfile::TempDir) {
    fs::write(temp.path().join("mention-target.txt"), "safe\n").unwrap();
    fs::write(temp.path().join("alpha-one.txt"), "one\n").unwrap();
    fs::write(temp.path().join("alpha-two.txt"), "two\n").unwrap();
    fs::write(temp.path().join(".env"), "TOKEN=secret\n").unwrap();

    let skill = temp.path().join(".agents/skills/atlas");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "---\nname: atlas\ndescription: Atlas\n---\nBody\n").unwrap();

    let memory = temp.path().join(".agents/memories");
    fs::create_dir_all(&memory).unwrap();
    fs::write(memory.join("testing.md"), "---\nname: testing\nimportance: high\n---\nRun quiet.\n")
        .unwrap();
}

#[test]
fn bare_at_tab_opens_the_mention_picker_and_inserts_the_selected_file() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "review @");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    let picker = app.picker.as_ref().expect("bare @ should open the mention picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentMentions);
    assert_eq!(picker.title, "Insert Mention");
    let labels = picker.visible_items_range(0, picker.visible_count());
    assert!(labels.contains(&String::from("@skill:atlas")), "{labels:?}");
    assert!(labels.contains(&String::from("@memory:testing")), "{labels:?}");
    assert!(labels.contains(&String::from("@mention-target.txt")), "{labels:?}");
    assert!(
        !labels.iter().any(|label| label.contains(".env")),
        "protected paths must not be suggested: {labels:?}"
    );

    type_text(&mut app, "mention-target");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.picker.is_none(), "selection closes the picker");
    assert_eq!(app.agents.threads[0].draft, "review @mention-target.txt");
}

#[test]
fn mention_picker_inserts_skill_and_memory_tokens() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "@");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    type_text(&mut app, "skill:atlas");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "@skill:atlas");

    type_text(&mut app, " @");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    type_text(&mut app, "memory:testing");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "@skill:atlas @memory:testing");
}

#[test]
fn ambiguous_path_prefix_opens_a_prefiltered_mention_picker() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "read @alpha-");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    let picker = app.picker.as_ref().expect("ambiguous prefix should open the picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentMentions);
    assert_eq!(picker.query, "alpha-");
    assert_eq!(picker.visible_count(), 2);

    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "read @alpha-one.txt");
}

#[test]
fn reference_picker_inserts_read_on_demand_tokens() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "check @ref:");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    let picker = app.picker.as_ref().expect("@ref: should open the reference picker");
    assert_eq!(picker.kind, crate::picker::PickerKind::AgentMentions);
    assert_eq!(picker.title, "Reference File");
    let labels = picker.visible_items_range(0, picker.visible_count());
    assert!(labels.contains(&String::from("@ref:mention-target.txt")), "{labels:?}");
    assert!(!labels.iter().any(|label| label == "@mention-target.txt"), "{labels:?}");
    assert!(!labels.iter().any(|label| label.contains(".env")), "{labels:?}");

    type_text(&mut app, "mention-target");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "check @ref:mention-target.txt");
}

#[test]
fn reference_prefix_completes_unique_file_without_a_picker() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "check @ref:mention-t");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    assert!(app.picker.is_none(), "unique reference must not open the picker");
    assert_eq!(app.agents.threads[0].draft, "check @ref:mention-target.txt");
}

#[test]
fn ambiguous_reference_prefix_opens_a_prefiltered_picker() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "check @ref:alpha-");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    let picker = app.picker.as_ref().expect("ambiguous reference prefix should open the picker");
    assert_eq!(picker.title, "Reference File");
    assert_eq!(picker.query, "alpha-");
    assert_eq!(picker.visible_count(), 2);

    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.agents.threads[0].draft, "check @ref:alpha-one.txt");
}

#[test]
fn unique_path_prefix_still_completes_without_a_picker() {
    let (mut app, temp, _fake) = fake_agents_app(base_script());
    open_pane_and_wait_ready(&mut app);
    mention_fixture(&temp);

    type_text(&mut app, "review @mention-t");
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);

    assert!(app.picker.is_none(), "unique completion must not open the picker");
    assert_eq!(app.agents.threads[0].draft, "review @mention-target.txt");
}
