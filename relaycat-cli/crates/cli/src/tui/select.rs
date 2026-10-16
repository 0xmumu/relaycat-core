use super::*;

/// Run the tool → project → relay wizard. Returns the confirmed selection, or
/// `None` if the user quit. May mutate and persist `config` (favorite toggles).
pub(crate) fn select_launch(
    config: &mut Config,
    config_path: Option<&Path>,
    language: CliLanguage,
    launcher_start: LauncherStart,
    update_notice_rx: &mpsc::Receiver<UpdateNotice>,
    update_notice: &mut Option<UpdateNotice>,
) -> Result<Option<Launch>> {
    let tools = build_tools(config, language);

    let mut guard = TerminalGuard::enter()?;
    let mut tool_state = ListState::default();
    tool_state.select(Some(default_tool_index(&tools, config)));
    let mut project_state = ListState::default();
    project_state.select(Some(0));

    let mut step = Step::Tool;
    let mut project: Option<PathBuf> = None;
    let mut relay_input = String::new();
    let mut custom_input = String::new();

    if let LauncherStart::Relay(launch) = launcher_start {
        tool_state.select(Some(tool_index_for_launch(&tools, &launch)));
        project = launch.project.clone();
        relay_input = launch.relay.unwrap_or_default();
        step = Step::Relay;
        let records = load_recent_records();
        let projects = project_choices(&records, config, language);
        project_state.select(Some(project_index_for_launch(
            &projects,
            project.as_deref(),
        )));
    }

    loop {
        poll_update_notice(update_notice_rx, update_notice);

        // Refresh recent records each time we return to the launcher so a
        // completed session updates the next relay/project defaults in the same
        // process.
        let mut records = load_recent_records();
        let mut projects = project_choices(&records, config, language);

        guard
            .terminal
            .draw(|frame| match step {
                Step::Tool => draw_picker_with_update_notice(
                    frame,
                    &tools,
                    &mut tool_state,
                    language,
                    update_notice.as_ref(),
                ),
                Step::Project => draw_project(
                    frame,
                    &selected_tool(&tools, &tool_state).label,
                    &projects,
                    &mut project_state,
                    language,
                    update_notice.as_ref(),
                ),
                Step::ProjectCustom => draw_project_custom(
                    frame,
                    &selected_tool(&tools, &tool_state).label,
                    &custom_input,
                    language,
                    update_notice.as_ref(),
                ),
                Step::Relay => draw_relay(
                    frame,
                    &selected_tool(&tools, &tool_state).label,
                    &relay_input,
                    language,
                    update_notice.as_ref(),
                ),
            })
            .context("failed to draw TUI")?;

        if !event::poll(UPDATE_NOTICE_POLL_INTERVAL).context("failed to poll terminal event")? {
            continue;
        }

        let ev = event::read().context("failed to read terminal event")?;

        // Handle paste events for text input steps.
        if let Event::Paste(text) = &ev {
            match step {
                Step::ProjectCustom => append_pasted_single_line(&mut custom_input, text),
                Step::Relay => append_pasted_single_line(&mut relay_input, text),
                _ => {}
            }
            continue;
        }

        let Event::Key(key) = ev else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if key.code == KeyCode::Char('c')
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return Ok(None);
        }

        match step {
            Step::Tool => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Up | KeyCode::Char('k') => {
                    move_selection(&mut tool_state, -1, tools.len())
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_selection(&mut tool_state, 1, tools.len())
                }
                KeyCode::Enter => step = Step::Project,
                _ => {}
            },
            Step::Project => match key.code {
                KeyCode::Esc => step = Step::Tool,
                KeyCode::Up | KeyCode::Char('k') => {
                    move_selection(&mut project_state, -1, projects.len())
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_selection(&mut project_state, 1, projects.len())
                }
                KeyCode::Char('f') => {
                    if let Some(path) = selected_project(&projects, &project_state)
                        .kind
                        .favorite_path()
                    {
                        config.toggle_favorite(path);
                        if let Some(config_path) = config_path {
                            let _ = config.save(config_path);
                        }
                        let selected = project_state.selected();
                        projects = project_choices(&records, config, language);
                        if let Some(selected) = selected {
                            project_state
                                .select(Some(selected.min(projects.len().saturating_sub(1))));
                        }
                    }
                }
                KeyCode::Char('d') => {
                    if let ProjectChoiceKind::Path(path) =
                        &selected_project(&projects, &project_state).kind
                    {
                        let path = path.clone();
                        // Remove from favorites if present.
                        config.remove_favorite(&path);
                        if let Some(config_path) = config_path {
                            let _ = config.save(config_path);
                        }
                        // Remove from recent records.
                        if let Ok(recent_path) = recent_store::recent_file_path()
                            && let Ok(mut store) = recent_store::RecentStore::load(&recent_path)
                        {
                            store.forget_project(&path);
                            let _ = store.save(&recent_path);
                        }
                        // Reload records and refresh the project list.
                        records = load_recent_records();
                        projects = project_choices(&records, config, language);
                        let selected = project_state.selected().unwrap_or(0);
                        project_state.select(Some(selected.min(projects.len().saturating_sub(1))));
                    }
                }
                KeyCode::Enter => match &selected_project(&projects, &project_state).kind {
                    ProjectChoiceKind::CurrentDir => {
                        project = None;
                        relay_input = default_relay_for(&records, None, config).unwrap_or_default();
                        step = Step::Relay;
                    }
                    ProjectChoiceKind::Path(path) => {
                        relay_input =
                            default_relay_for(&records, Some(path), config).unwrap_or_default();
                        project = Some(path.clone());
                        step = Step::Relay;
                    }
                    ProjectChoiceKind::Custom => {
                        custom_input.clear();
                        step = Step::ProjectCustom;
                    }
                },
                _ => {}
            },
            Step::ProjectCustom => match key.code {
                KeyCode::Esc => step = Step::Project,
                KeyCode::Enter => {
                    let trimmed = custom_input.trim();
                    let chosen = (!trimmed.is_empty()).then(|| PathBuf::from(trimmed));
                    relay_input =
                        default_relay_for(&records, chosen.as_deref(), config).unwrap_or_default();
                    project = chosen;
                    step = Step::Relay;
                }
                _ => apply_single_line_key_input(&mut custom_input, key.code, key.modifiers),
            },
            Step::Relay => match key.code {
                KeyCode::Esc => step = Step::Project,
                KeyCode::Enter => {
                    let tool = selected_tool(&tools, &tool_state).tool.clone();
                    let trimmed = relay_input.trim();
                    let relay = (!trimmed.is_empty()).then(|| trimmed.to_string());
                    return Ok(Some(Launch {
                        tool,
                        project: project.clone(),
                        relay,
                    }));
                }
                _ => apply_single_line_key_input(&mut relay_input, key.code, key.modifiers),
            },
        }
    }
}

pub(crate) fn append_pasted_single_line(buffer: &mut String, text: &str) {
    buffer.extend(text.chars().filter(|ch| !ch.is_control()));
}

pub(crate) fn apply_single_line_key_input(
    buffer: &mut String,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) {
    use crossterm::event::KeyModifiers;

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('u' | 'U') => buffer.clear(),
            KeyCode::Char('h' | 'H') => {
                buffer.pop();
            }
            KeyCode::Char('w' | 'W') => {
                while buffer.ends_with(char::is_whitespace) {
                    buffer.pop();
                }
                while let Some(ch) = buffer.chars().last() {
                    if ch.is_whitespace() {
                        break;
                    }
                    buffer.pop();
                }
            }
            _ => {}
        }
        return;
    }
    if modifiers.contains(KeyModifiers::ALT) || modifiers.contains(KeyModifiers::SUPER) {
        return;
    }

    match code {
        KeyCode::Backspace => {
            buffer.pop();
        }
        KeyCode::Char(c) if !c.is_control() => buffer.push(c),
        _ => {}
    }
}

pub(crate) fn move_selection(state: &mut ListState, delta: isize, len: usize) {
    let len = len as isize;
    if len == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).rem_euclid(len);
    state.select(Some(next as usize));
}
