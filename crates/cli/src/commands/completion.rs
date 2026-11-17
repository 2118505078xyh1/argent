//! Shell completion: the command/subcommand tables and the generated script.

use super::*;

pub(super) const COMMANDS: &[&str] = &[
    "login",
    "logout",
    "auth",
    "models",
    "mcp",
    "plugins",
    "skills",
    "commands",
    "hooks",
    "workflow",
    "telemetry",
    "sessions",
    "audit",
    "usage",
    "policy",
    "profiles",
    "themes",
    "keybindings",
    "automation",
    "eval",
    "update",
    "completion",
    "agents",
    "worktree",
    "replay",
    "attach",
    "logs",
    "follow",
    "stop",
    "rm",
    "doctor",
    "bug",
    "help",
    "version",
];

/// The second level of the commands that take one.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "sessions",
        &[
            "list",
            "search",
            "export",
            "archive",
            "unarchive",
            "delete",
            "prune",
        ],
    ),
    ("update", &["check", "apply", "rollback", "keygen", "sign"]),
    ("mcp", &["list", "add", "remove"]),
    ("worktree", &["list", "remove", "prune", "clean"]),
    ("agents", &["list", "stop", "rm"]),
    ("logs", &["follow"]),
    ("auth", &["login", "logout", "status"]),
];

/// Build the completion script for a shell.
pub(super) fn completion_script(shell: &str) -> Result<String> {
    let commands = COMMANDS.join(" ");
    match shell {
        "bash" => {
            let mut cases = String::new();
            for (name, subs) in SUBCOMMANDS {
                cases.push_str(&format!(
                    "        {name}) COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ) ;;\n",
                    subs.join(" ")
                ));
            }
            Ok(format!(
                r#"# argent bash completion
_argent() {{
    local cur="${{COMP_WORDS[COMP_CWORD]}}"
    local prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "{commands}" -- "$cur") )
        return
    fi
    case "$prev" in
{cases}        *) COMPREPLY=( $(compgen -f -- "$cur") ) ;;
    esac
}}
complete -F _argent argent"#
            ))
        }
        "zsh" => {
            let mut cases = String::new();
            for (name, subs) in SUBCOMMANDS {
                cases.push_str(&format!(
                    "        {name}) commands=({}) ;;\n",
                    subs.join(" ")
                ));
            }
            Ok(format!(
                r#"#compdef argent
_argent() {{
    local -a commands
    commands=({commands})
    if (( CURRENT == 2 )); then
        _describe 'command' commands
        return
    fi
    case "${{words[2]}}" in
{cases}        *) _files ;;
    esac
}}
_argent"#
            ))
        }
        "fish" => {
            let mut lines = Vec::new();
            for command in COMMANDS {
                lines.push(format!(
                    "complete -c argent -n '__fish_use_subcommand' -a {}",
                    command
                ));
            }
            for (name, subs) in SUBCOMMANDS {
                for sub in *subs {
                    lines.push(format!(
                        "complete -c argent -n '__fish_seen_subcommand_from {}' -a {}",
                        name, sub
                    ));
                }
            }
            Ok(lines.join("\n"))
        }
        "powershell" | "pwsh" => {
            let mut cases = String::new();
            for (name, subs) in SUBCOMMANDS {
                cases.push_str(&format!(
                    "            '{name}' {{ $subs = '{}' -split ' ' }}",
                    subs.join(" ")
                ));
            }
            Ok(format!(
                r#"Register-ArgumentCompleter -Native -CommandName argent -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)
    $subs = $null
    if ($commandAst.CommandElements.Count -gt 2) {{
        switch ($commandAst.CommandElements[1].Value) {{
{cases}
        }}
    }}
    if ($subs) {{
        $subs | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
        }}
        return
    }}
    $commands = '{commands}' -split ' '
    $commands | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}"#
            ))
        }
        other => Err(Error::config(format!(
            "unsupported shell '{}'; expected bash, zsh, fish or powershell",
            other
        ))),
    }
}

/// `argent completion <shell>`：输出补全脚本。
pub(super) fn completion(args: &[String]) -> Result<i32> {
    let shell = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("bash");
    println!("{}", completion_script(shell)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_gets_a_script() {
        for shell in ["bash", "zsh", "fish", "powershell", "pwsh"] {
            let script = completion_script(shell).expect("script");
            assert!(script.contains("argent"), "{}: {}", shell, script);
            assert!(script.len() > 50, "{}: too short", shell);
        }
    }

    #[test]
    fn the_top_level_commands_are_offered() {
        let script = completion_script("bash").expect("script");
        for command in ["sessions", "attach", "agents", "update", "worktree", "stop"] {
            assert!(script.contains(command), "{} missing", command);
        }
    }

    #[test]
    fn the_second_level_is_offered() {
        let bash = completion_script("bash").expect("script");
        assert!(bash.contains("search"), "sessions search missing");
        assert!(bash.contains("rollback"), "update rollback missing");
        let zsh = completion_script("zsh").expect("script");
        assert!(zsh.contains("prune"), "worktree prune missing");
        let fish = completion_script("fish").expect("script");
        assert!(
            fish.contains("__fish_seen_subcommand_from sessions"),
            "fish: {}",
            fish
        );
    }

    #[test]
    fn an_unknown_shell_is_refused() {
        let error = completion_script("csh").expect_err("should refuse");
        assert!(error.to_string().contains("unsupported shell"));
    }

    #[test]
    fn the_command_table_covers_the_dispatcher() {
        // Every command the dispatcher answers has to be offered here, or the
        // completion would hide a command that works.
        for command in [
            "login",
            "logout",
            "auth",
            "models",
            "mcp",
            "plugins",
            "skills",
            "commands",
            "hooks",
            "workflow",
            "telemetry",
            "sessions",
            "audit",
            "usage",
            "policy",
            "profiles",
            "themes",
            "keybindings",
            "automation",
            "eval",
            "update",
            "completion",
            "agents",
            "worktree",
            "attach",
            "logs",
            "follow",
            "stop",
            "rm",
            "doctor",
            "bug",
            "help",
            "version",
        ] {
            assert!(
                COMMANDS.contains(&command),
                "{} is not in the completion table",
                command
            );
        }
    }
}
