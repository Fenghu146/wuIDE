# Bounded execution for the steps that can stall with no output at all. Dot-sourced
# by the bundlers and by `generate-licenses`, not run on its own.

# Run a native command with a hard time limit, killing its whole tree when it
# outlives that limit. Both callers must fail loudly instead of hanging a release
# job, and Windows has no timeout(1) that reaches a child tree.
function Invoke-WithTimeout {
    param(
        [Parameter(Mandatory)][string[]]$Command,
        [Parameter(Mandatory)][int]$TimeoutSeconds,
        [Parameter(Mandatory)][string]$TimeoutMessage
    )

    $commandLine = $Command -join ' '
    $argumentList = ($Command | Select-Object -Skip 1 | ForEach-Object { if ($_ -match '\s') { "`"$_`"" } else { $_ } }) -join ' '
    $process = Start-Process -FilePath $Command[0] -ArgumentList $argumentList -NoNewWindow -PassThru
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        try { $process.Kill($true) } catch { $process.Kill() }
        Write-Host $TimeoutMessage
        throw $TimeoutMessage
    }
    if ($process.ExitCode -ne 0) {
        throw "$commandLine failed with exit code $($process.ExitCode)"
    }
}

# Download the lockfile's crates and git sources as a step of its own, before
# anything needs them. `cargo about` fetches them itself through `cargo metadata`
# with its output captured, and that fetch is where 1.4.7's bundle_mac (aarch64)
# printed nothing for 4h55m: done here, it shows progress, and when it does stall
# the job says so by name instead of the next step hanging silently.
function Invoke-CratePrefetch {
    $limit = if ($env:CARGO_FETCH_TIMEOUT_SECONDS) { [int]$env:CARGO_FETCH_TIMEOUT_SECONDS } else { 600 }
    Invoke-WithTimeout -Command @('cargo', 'fetch', '--locked') -TimeoutSeconds $limit -TimeoutMessage "cargo fetch --locked timed out after ${limit}s"
}
