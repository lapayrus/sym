# Security policy

sym runs locally. It reads the source files of the repository it indexes, writes `.sym/index.db`
inside that repository, and talks to its MCP client over stdin/stdout. It makes no network connections.

## Reporting a vulnerability

Please **don't open a public issue**. Instead, report it privately through GitHub:
**Security** tab → **Report a vulnerability** on https://github.com/lapayrus/sym.

Include the version (`sym --version`), your OS, and steps to reproduce. You should get a reply within
a week. A fix is released as a patch version, and the advisory is published once the fix is out.

## Supported versions

Only the latest release gets security fixes.
