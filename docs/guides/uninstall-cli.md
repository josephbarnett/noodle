# noodle uninstall — single-step CLI reference

Each line below runs **exactly one** uninstall step then quits.
Run them in order and verify each independently (e.g.
`systemextensionsctl list` after `--uninstall-deactivate-sysext`,
`security find-certificate -a -c ca.noodleproxy.macos` after
`--uninstall-purge-ca`). None of these activate the sysext.

See `apps/noodle-macos/Container/main.swift::UninstallStepArg`
for the source of truth.

```bash
open -n /Applications/Noodle.app --args --uninstall-stop-proxy
open -n /Applications/Noodle.app --args --uninstall-remove-managers
open -n /Applications/Noodle.app --args --uninstall-deactivate-sysext   # the critical one
open -n /Applications/Noodle.app --args --uninstall-purge-ca
open -n /Applications/Noodle.app --args --uninstall-unset-env
open -n /Applications/Noodle.app --args --uninstall-clear-defaults
open -n /Applications/Noodle.app --args --uninstall-trash-app
```

For a one-shot non-interactive uninstall (script-side cleanup
only; SIP prevents script-driven sysext deactivation — reboot
required afterwards), see
[`../../apps/noodle-macos/scripts/noodle-uninstall.sh`](../../apps/noodle-macos/scripts/noodle-uninstall.sh).
