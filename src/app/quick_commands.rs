use super::*;

pub(super) fn all_quick_group_names(store: &ConfigStore) -> std::collections::HashSet<String> {
    let cmds = store.quick_commands();
    let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
    if cmds.iter().any(|c| c.group.trim().is_empty()) {
        set.insert("default".to_string());
    }
    for g in store.quick_groups() {
        set.insert(g.clone());
    }
    for c in cmds {
        let g = c.group.trim();
        if !g.is_empty() {
            set.insert(g.to_string());
        }
    }
    set
}

/// Build the quick-command model for the command bar + manage dialog (#55).
///
/// Grouped like the welcome session list: the implicit "default" group (entries
/// with an empty group) comes first, then named groups alphabetically. Within a
/// group, entries keep their saved order. `group_header` is set on the first row
/// of each group; `collapsed` reflects `collapsed_groups` (runtime-only state);
/// `orig_index` points back into the stored vec so deletes target the right entry
/// even though the display order differs.
pub(super) fn quick_cmd_model(
    store: &ConfigStore,
    collapsed_groups: &std::collections::HashSet<String>,
) -> ModelRc<QuickCmd> {
    let cmds = store.quick_commands();

    let has_default = cmds.iter().any(|c| c.group.trim().is_empty());
    // Named groups = explicit quick-groups ∪ groups referenced by commands.
    let mut named: Vec<String> = store
        .quick_groups()
        .iter()
        .cloned()
        .chain(
            cmds.iter()
                .map(|c| c.group.trim().to_string())
                .filter(|g| !g.is_empty()),
        )
        .collect();
    named.sort_by_key(|g| g.to_lowercase());
    named.dedup();

    let mut groups: Vec<String> = Vec::new();
    if has_default {
        groups.push("default".to_string());
    }
    groups.extend(named);

    let mut rows: Vec<QuickCmd> = Vec::new();
    for group in &groups {
        let is_collapsed = collapsed_groups.contains(group);
        let members: Vec<(usize, &crate::config::QuickCommand)> = cmds
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let g = c.group.trim();
                if group == "default" {
                    g.is_empty()
                } else {
                    g == group
                }
            })
            .collect();
        if members.is_empty() {
            // Header-only placeholder for an empty group (orig_index -1) so it can
            // still be renamed / deleted, matching empty session folders.
            rows.push(QuickCmd {
                name: "".into(),
                command: "".into(),
                group: group.clone().into(),
                group_header: group.clone().into(),
                collapsed: is_collapsed,
                orig_index: -1,
                send_enter: true,
                command_preview: "".into(),
            });
        } else {
            for (i, (orig_idx, c)) in members.iter().enumerate() {
                rows.push(QuickCmd {
                    name: c.name.clone().into(),
                    command: c.command.clone().into(),
                    group: group.clone().into(),
                    group_header: if i == 0 {
                        group.clone().into()
                    } else {
                        "".into()
                    },
                    collapsed: is_collapsed,
                    orig_index: *orig_idx as i32,
                    send_enter: c.send_enter,
                    command_preview: command_preview(&c.command).into(),
                });
            }
        }
    }
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// One-line rendering of a quick command for lists and chips (#419).
///
/// Multi-line commands (scripts, `for` loops pasted from a .bat) used to be
/// drawn with their real line breaks inside a fixed-height row and spilled
/// over the neighbouring entries. Line breaks become a visible ` ⏎ ` marker,
/// runs of whitespace collapse to one space, and very long commands are cut
/// so the UI never lays out kilobytes of text for an elided label.
pub(super) fn command_preview(command: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut out = String::new();
    for (i, line) in command
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if i > 0 {
            out.push_str(" \u{23CE} ");
        }
        out.push_str(&line);
        if out.chars().count() > MAX_CHARS {
            break;
        }
    }
    if out.chars().count() > MAX_CHARS {
        out = out.chars().take(MAX_CHARS).collect();
        out.push('…');
    }
    out
}

/// Where the entry being edited ends up after deleting `deleted` (#419):
/// `-1` (form reset) if it was the deleted one, shifted down if it was after it.
pub(super) fn edit_index_after_delete(edit_index: i32, deleted: i32) -> i32 {
    if edit_index < 0 || deleted < 0 {
        edit_index
    } else if edit_index == deleted {
        -1
    } else if edit_index > deleted {
        edit_index - 1
    } else {
        edit_index
    }
}

/// Where the entry being edited ends up after `a` and `b` swap places.
pub(super) fn edit_index_after_swap(edit_index: i32, a: usize, b: usize) -> i32 {
    if edit_index == a as i32 {
        b as i32
    } else if edit_index == b as i32 {
        a as i32
    } else {
        edit_index
    }
}

/// Move entry `index` one step within its group. Returns the index it was
/// swapped with, or `None` if it was already at the edge of its group.
pub(super) fn reorder_quick_command(
    commands: &mut [crate::config::QuickCommand],
    index: usize,
    move_up: bool,
) -> Option<usize> {
    let Some(current) = commands.get(index) else {
        return None;
    };
    let group = current.group.trim().to_string();
    let target = if move_up {
        (0..index)
            .rev()
            .find(|&candidate| commands[candidate].group.trim() == group)
    } else {
        (index + 1..commands.len())
            .find(|&candidate| commands[candidate].group.trim() == group)
    };
    if let Some(target) = target {
        commands.swap(index, target);
    }
    target
}

#[cfg(test)]
mod reorder_tests {
    use super::reorder_quick_command;
    use crate::config::QuickCommand;

    fn command(name: &str, group: &str) -> QuickCommand {
        QuickCommand {
            name: name.to_string(),
            command: name.to_string(),
            group: group.to_string(),
            send_enter: true,
        }
    }

    #[test]
    fn reorders_only_within_the_current_group() {
        let mut commands = vec![
            command("a", "ops"),
            command("x", "other"),
            command("b", "ops"),
        ];
        assert_eq!(reorder_quick_command(&mut commands, 2, true), Some(0));
        assert_eq!(
            commands.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "x", "a"]
        );
        assert_eq!(reorder_quick_command(&mut commands, 0, true), None);
    }
}

#[cfg(test)]
mod preview_tests {
    use super::command_preview;

    #[test]
    fn single_line_command_is_unchanged() {
        assert_eq!(command_preview("docker exec -it goose-cli bash"), "docker exec -it goose-cli bash");
    }

    #[test]
    fn multi_line_command_becomes_one_line() {
        let cmd = "(for /f \"tokens=5\" %a in ('netstat -ano') do (\r\n    taskkill /F /PID %a\r\n))\r\n";
        let preview = command_preview(cmd);
        assert!(!preview.contains('\n') && !preview.contains('\r'));
        assert_eq!(
            preview,
            "(for /f \"tokens=5\" %a in ('netstat -ano') do ( \u{23CE} taskkill /F /PID %a \u{23CE} ))"
        );
    }

    #[test]
    fn blank_lines_and_tabs_collapse() {
        assert_eq!(command_preview("\n\tls   -la\n\n\tpwd\n"), "ls -la \u{23CE} pwd");
    }

    #[test]
    fn pasted_top_output_renders_as_one_line() {
        let pasted = "7 root       0 -20       0      0      0 I   0.0   0.0   0:00.00 kworker/R-netns\n      9 root       0 -20       0      0      0 I   0.0   0.0   0:00.00 kworker/0:0H-events_hig+\n     12 root       0 -20       0      0      0 I   0.0   0.0   0:00.00 kworker/R-mm_pe\n";
        let preview = command_preview(pasted);
        assert!(!preview.contains('\n'));
        assert!(preview.starts_with("7 root 0 -20 0 0 0 I 0.0 0.0 0:00.00 kworker/R-netns \u{23CE} 9 root"));
    }

    #[test]
    fn very_long_command_is_truncated() {
        let preview = command_preview(&"x".repeat(1000));
        assert_eq!(preview.chars().count(), 241);
        assert!(preview.ends_with('…'));
    }
}

#[cfg(test)]
mod edit_index_tests {
    use super::{edit_index_after_delete, edit_index_after_swap};

    #[test]
    fn delete_keeps_the_edited_entry_selected() {
        assert_eq!(edit_index_after_delete(3, 1), 2);
        assert_eq!(edit_index_after_delete(3, 5), 3);
        assert_eq!(edit_index_after_delete(3, 3), -1);
        assert_eq!(edit_index_after_delete(-1, 0), -1);
    }

    #[test]
    fn swap_follows_the_edited_entry() {
        assert_eq!(edit_index_after_swap(2, 2, 0), 0);
        assert_eq!(edit_index_after_swap(0, 2, 0), 2);
        assert_eq!(edit_index_after_swap(1, 2, 0), 1);
    }
}
