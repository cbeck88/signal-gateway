# signal-gateway-code-tool

A tool that can be provided to the `signal-gateway-assistant`, which allows it
to fetch and read code without giving it shell access.

If configured with a github read-only access token, and if you give it a route
to query the current git SHA of your deployed binary, it can transparently fetch the code
corresponding to that.
