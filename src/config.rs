//! The text of `--config FILE` and `--config-string`: cut into the commands that
//! the shim gives to tmux one by one, so that a command that tmux does not
//! accept does not take the others with it, and the error can say where it is.
//!
//! Pure, with no I/O, like the filter. A file has one command per line (blank
//! lines and lines that start with `#` are skipped); a string has its commands
//! separated by `;`. Nothing else is looked at: the command is for tmux.

/// The commands of a file, each with the number of its line (from 1).
pub fn file_commands(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(number, line)| (number, line.to_string()))
        .collect()
}

/// The commands of a string, each with its number in the string (from 1).
pub fn string_commands(text: &str) -> Vec<(usize, String)> {
    text.split(';')
        .enumerate()
        .map(|(index, command)| (index + 1, command.trim()))
        .filter(|(_, command)| !command.is_empty())
        .map(|(number, command)| (number, command.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_has_one_command_per_line() {
        let text = "# the mouse\nset -g mouse on   \n\n  # more\n  setw -g mode-keys vi\n";
        assert_eq!(
            file_commands(text),
            [
                (2, "set -g mouse on".to_string()),
                (5, "setw -g mode-keys vi".to_string())
            ]
        );
        // A # in the command is not a comment, and a ; is for tmux.
        assert_eq!(
            file_commands("set -g status-left '#S'; set -g x y"),
            [(1, "set -g status-left '#S'; set -g x y".to_string())]
        );
    }

    #[test]
    fn a_string_has_its_commands_between_semicolons() {
        assert_eq!(
            string_commands("set -g mouse on; setw -g mode-keys vi;set -g set-clipboard on ; "),
            [
                (1, "set -g mouse on".to_string()),
                (2, "setw -g mode-keys vi".to_string()),
                (3, "set -g set-clipboard on".to_string())
            ]
        );
    }

    #[test]
    fn nothing_to_do_is_nothing() {
        assert!(file_commands("").is_empty());
        assert!(file_commands(" \n# only\n").is_empty());
        assert!(string_commands(" ; ;").is_empty());
    }
}
