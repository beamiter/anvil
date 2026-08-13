# PowerShell argument completion for anvil

Register-ArgumentCompleter -Native -CommandName anvil -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $words = @($commandAst.CommandElements | ForEach-Object { $_.Extent.Text })
    $previous = if ($words.Count -ge 2) { $words[-2] } else { '' }

    $values = switch ($previous) {
        '--mode' { @('block', 'vte', 'unified'); break }
        '--shell-integration' { @('bash', 'zsh', 'fish', 'pwsh'); break }
        '--generate-completion' { @('bash', 'zsh', 'fish', 'pwsh'); break }
        '--completion' { @('bash', 'zsh', 'fish', 'pwsh'); break }
        default {
            @(
                '-h', '--help', '-V', '--version',
                '-c', '--config', '-d', '--working-directory',
                '-e', '--execute', '--mode', '--no-restore', '--safe-mode',
                '--doctor', '--json', '--check-config',
                '--restore-config-backup', '--config-path', '--init-config',
                '--print-default-config', '--shell-integration',
                '--generate-completion', '--completion'
            )
        }
    }

    $values |
        Where-Object { $_ -like "$wordToComplete*" } |
        ForEach-Object {
            [System.Management.Automation.CompletionResult]::new(
                $_, $_, 'ParameterValue', $_
            )
        }
}
