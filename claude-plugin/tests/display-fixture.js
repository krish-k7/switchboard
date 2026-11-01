// What each command shows, as the CLI's --json --display adds it. Written by
// switchboard/src/cli/view/tests.rs: SWITCHBOARD_BLESS=1 cargo test the_plugin_fixture_matches
// The plugin's tests check that it draws every word of every case.
export default {
  "slash_commands": [
    "status",
    "chats",
    "join",
    "leave",
    "ping",
    "up",
    "down",
    "doctor",
    "help"
  ],
  "cases": [
    {
      "name": "status-connected-in-chat",
      "words": [
        "Connected to sb.example.com as laptop",
        "This chat",
        "Joined as release-planner, expires in 6 days",
        "Chats here",
        "1 of 2 joined"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Connected to sb.example.com as laptop",
          "tone": "success"
        },
        {
          "kind": "fact",
          "label": "This chat",
          "value": "Joined as release-planner, expires in 6 days"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        }
      ]
    },
    {
      "name": "status-connected-chat-not-joined",
      "words": [
        "Connected to sb.example.com as laptop",
        "This chat",
        "Not joined. Run /switchboard join.",
        "Chats here",
        "1 of 2 joined"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Connected to sb.example.com as laptop",
          "tone": "success"
        },
        {
          "kind": "fact",
          "label": "This chat",
          "value": "Not joined. Run /switchboard join."
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        }
      ]
    },
    {
      "name": "status-connected-terminal",
      "words": [
        "Connected to sb.example.com as laptop",
        "Chats here",
        "1 of 2 joined"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Connected to sb.example.com as laptop",
          "tone": "success"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        }
      ]
    },
    {
      "name": "status-none-joined",
      "words": [
        "Connected to sb.example.com as laptop",
        "Chats here",
        "0 of 1 joined",
        "To connect a chat, run this in it:",
        "/switchboard join"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Connected to sb.example.com as laptop",
          "tone": "success"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "0 of 1 joined"
        },
        {
          "kind": "next",
          "space": true,
          "text": "To connect a chat, run this in it:",
          "command": "/switchboard join"
        }
      ]
    },
    {
      "name": "status-update",
      "words": [
        "Connected to sb.example.com as laptop",
        "This chat",
        "Joined as release-planner, expires in 6 days",
        "Chats here",
        "1 of 2 joined",
        "Switchboard 0.3.1 is available. Run `switchboard upgrade`."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Connected to sb.example.com as laptop",
          "tone": "success"
        },
        {
          "kind": "fact",
          "label": "This chat",
          "value": "Joined as release-planner, expires in 6 days"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        },
        {
          "kind": "text",
          "text": "Switchboard 0.3.1 is available. Run `switchboard upgrade`.",
          "space": true
        }
      ]
    },
    {
      "name": "status-not-connected",
      "words": [
        "Not connected to sb.example.com: the bridge reports it is offline.",
        "This chat",
        "Joined as release-planner, expires in 6 days",
        "Chats here",
        "1 of 2 joined",
        "To see why, run",
        "switchboard doctor"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Not connected to sb.example.com: the bridge reports it is offline.",
          "tone": "error"
        },
        {
          "kind": "fact",
          "label": "This chat",
          "value": "Joined as release-planner, expires in 6 days"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        },
        {
          "kind": "next",
          "space": true,
          "text": "To see why, run",
          "command": "switchboard doctor"
        }
      ]
    },
    {
      "name": "status-not-running",
      "words": [
        "Not connected: the bridge isn't running.",
        "Chats here",
        "1 of 2 joined",
        "To reconnect, run",
        "switchboard up"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Not connected: the bridge isn't running.",
          "tone": "error"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        },
        {
          "kind": "next",
          "space": true,
          "text": "To reconnect, run",
          "command": "switchboard up"
        }
      ]
    },
    {
      "name": "status-no-service",
      "words": [
        "Not connected: the bridge isn't running.",
        "Chats here",
        "1 of 2 joined",
        "To install the bridge's service and start it, run",
        "switchboard install --start"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Not connected: the bridge isn't running.",
          "tone": "error"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        },
        {
          "kind": "next",
          "space": true,
          "text": "To install the bridge's service and start it, run",
          "command": "switchboard install --start"
        }
      ]
    },
    {
      "name": "status-turned-off",
      "words": [
        "Turned off in the bridge configuration, so this machine is offline.",
        "Chats here",
        "1 of 2 joined",
        "To turn it on, set \"enabled\" to true in the bridge configuration, then run",
        "switchboard up"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Turned off in the bridge configuration, so this machine is offline.",
          "tone": "warning"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        },
        {
          "kind": "next",
          "space": true,
          "text": "To turn it on, set \"enabled\" to true in the bridge configuration, then run",
          "command": "switchboard up"
        }
      ]
    },
    {
      "name": "status-down",
      "words": [
        "Down. Run `switchboard up` to reconnect.",
        "This chat",
        "Joined as release-planner, expires in 6 days",
        "Chats here",
        "1 of 2 joined"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Down. Run `switchboard up` to reconnect."
        },
        {
          "kind": "fact",
          "label": "This chat",
          "value": "Joined as release-planner, expires in 6 days"
        },
        {
          "kind": "fact",
          "label": "Chats here",
          "value": "1 of 2 joined"
        }
      ]
    },
    {
      "name": "status-not-set-up",
      "words": [
        "Not set up. Run `switchboard init`."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Not set up. Run `switchboard init`."
        }
      ]
    },
    {
      "name": "chats",
      "words": [
        "Chats",
        "laptop (this machine)",
        "Chat",
        "Activity",
        "Membership",
        "Expires",
        "Local work",
        "(this chat)",
        "Idle",
        "Joined as release-planner",
        "in 6 days",
        "Shell work",
        "Shell",
        "Not joined",
        "worker",
        "Chat",
        "Activity",
        "Remote research",
        "Busy",
        "Full IDs and details:",
        "switchboard chats --details"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Chats"
        },
        {
          "kind": "heading",
          "text": "laptop (this machine)",
          "space": true
        },
        {
          "kind": "table",
          "columns": [
            "Chat",
            "Activity",
            "Membership",
            "Expires"
          ],
          "rows": [
            {
              "cells": [
                "Local work",
                "Idle",
                "Joined as release-planner",
                "in 6 days"
              ],
              "note": "(this chat)"
            },
            {
              "cells": [
                "Shell work",
                "Shell",
                "Not joined",
                ""
              ]
            }
          ]
        },
        {
          "kind": "heading",
          "text": "worker",
          "space": true
        },
        {
          "kind": "table",
          "columns": [
            "Chat",
            "Activity"
          ],
          "rows": [
            {
              "cells": [
                "Remote research",
                "Busy"
              ]
            }
          ]
        },
        {
          "kind": "next",
          "space": true,
          "text": "Full IDs and details:",
          "command": "switchboard chats --details"
        }
      ]
    },
    {
      "name": "chats-details",
      "words": [
        "Chats",
        "laptop (this machine)",
        "Local work (this chat)",
        "Activity",
        "Idle",
        "Membership",
        "Joined",
        "Joined as",
        "release-planner",
        "Expires",
        "in 6 days",
        "Project",
        "Local project",
        "Repository",
        "example/monorepo",
        "Branch",
        "tools/example",
        "ID",
        "11111111-1111-4111-8111-111111111111",
        "Shell work",
        "Activity",
        "Shell",
        "Membership",
        "Not joined",
        "ID",
        "22222222-2222-4222-8222-222222222222",
        "worker",
        "Remote research",
        "Activity",
        "Busy",
        "Project",
        "Remote project",
        "Repository",
        "example/monorepo",
        "Branch",
        "research/example",
        "ID",
        "33333333-3333-4333-8333-333333333333"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Chats"
        },
        {
          "kind": "heading",
          "text": "laptop (this machine)",
          "space": true
        },
        {
          "kind": "text",
          "text": "Local work (this chat)",
          "space": true,
          "strong": true
        },
        {
          "kind": "fact",
          "label": "Activity",
          "value": "Idle"
        },
        {
          "kind": "fact",
          "label": "Membership",
          "value": "Joined"
        },
        {
          "kind": "fact",
          "label": "Joined as",
          "value": "release-planner"
        },
        {
          "kind": "fact",
          "label": "Expires",
          "value": "in 6 days"
        },
        {
          "kind": "fact",
          "label": "Project",
          "value": "Local project"
        },
        {
          "kind": "fact",
          "label": "Repository",
          "value": "example/monorepo"
        },
        {
          "kind": "fact",
          "label": "Branch",
          "value": "tools/example"
        },
        {
          "kind": "fact",
          "label": "ID",
          "value": "11111111-1111-4111-8111-111111111111"
        },
        {
          "kind": "text",
          "text": "Shell work",
          "space": true,
          "strong": true
        },
        {
          "kind": "fact",
          "label": "Activity",
          "value": "Shell"
        },
        {
          "kind": "fact",
          "label": "Membership",
          "value": "Not joined"
        },
        {
          "kind": "fact",
          "label": "ID",
          "value": "22222222-2222-4222-8222-222222222222"
        },
        {
          "kind": "heading",
          "text": "worker",
          "space": true
        },
        {
          "kind": "text",
          "text": "Remote research",
          "space": true,
          "strong": true
        },
        {
          "kind": "fact",
          "label": "Activity",
          "value": "Busy"
        },
        {
          "kind": "fact",
          "label": "Project",
          "value": "Remote project"
        },
        {
          "kind": "fact",
          "label": "Repository",
          "value": "example/monorepo"
        },
        {
          "kind": "fact",
          "label": "Branch",
          "value": "research/example"
        },
        {
          "kind": "fact",
          "label": "ID",
          "value": "33333333-3333-4333-8333-333333333333"
        }
      ]
    },
    {
      "name": "chats-local-search",
      "words": [
        "Chats on this machine matching “solver”",
        "laptop (this machine)",
        "Chat",
        "Activity",
        "Membership",
        "Expires",
        "Local work",
        "(this chat)",
        "Idle",
        "Joined as release-planner",
        "in 6 days",
        "Shell work",
        "Shell",
        "Not joined",
        "Full IDs and details:",
        "switchboard chats --local --details"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Chats on this machine matching “solver”"
        },
        {
          "kind": "heading",
          "text": "laptop (this machine)",
          "space": true
        },
        {
          "kind": "table",
          "columns": [
            "Chat",
            "Activity",
            "Membership",
            "Expires"
          ],
          "rows": [
            {
              "cells": [
                "Local work",
                "Idle",
                "Joined as release-planner",
                "in 6 days"
              ],
              "note": "(this chat)"
            },
            {
              "cells": [
                "Shell work",
                "Shell",
                "Not joined",
                ""
              ]
            }
          ]
        },
        {
          "kind": "next",
          "space": true,
          "text": "Full IDs and details:",
          "command": "switchboard chats --local --details"
        }
      ]
    },
    {
      "name": "chats-remote-unavailable",
      "words": [
        "Chats",
        "laptop (this machine)",
        "Chat",
        "Activity",
        "Membership",
        "Expires",
        "Local work",
        "(this chat)",
        "Idle",
        "Joined as release-planner",
        "in 6 days",
        "Shell work",
        "Shell",
        "Not joined",
        "Chats on other machines are unavailable: the server didn't answer within 8 seconds.",
        "Full IDs and details:",
        "switchboard chats --details"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Chats"
        },
        {
          "kind": "heading",
          "text": "laptop (this machine)",
          "space": true
        },
        {
          "kind": "table",
          "columns": [
            "Chat",
            "Activity",
            "Membership",
            "Expires"
          ],
          "rows": [
            {
              "cells": [
                "Local work",
                "Idle",
                "Joined as release-planner",
                "in 6 days"
              ],
              "note": "(this chat)"
            },
            {
              "cells": [
                "Shell work",
                "Shell",
                "Not joined",
                ""
              ]
            }
          ]
        },
        {
          "kind": "text",
          "space": true,
          "tone": "warning",
          "text": "Chats on other machines are unavailable: the server didn't answer within 8 seconds."
        },
        {
          "kind": "next",
          "space": true,
          "text": "Full IDs and details:",
          "command": "switchboard chats --details"
        }
      ]
    },
    {
      "name": "chats-nothing-here",
      "words": [
        "Chats matching “nothing”",
        "No matching chats on this machine.",
        "Chats on other machines are unavailable: the server didn't answer within 8 seconds."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Chats matching “nothing”"
        },
        {
          "kind": "text",
          "text": "No matching chats on this machine.",
          "space": true
        },
        {
          "kind": "text",
          "space": true,
          "tone": "warning",
          "text": "Chats on other machines are unavailable: the server didn't answer within 8 seconds."
        }
      ]
    },
    {
      "name": "chats-remote-only-unavailable",
      "words": [
        "Chats on other machines",
        "Chats on other machines are unavailable: the server didn't answer within 8 seconds."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Chats on other machines"
        },
        {
          "kind": "text",
          "space": true,
          "tone": "warning",
          "text": "Chats on other machines are unavailable: the server didn't answer within 8 seconds."
        }
      ]
    },
    {
      "name": "doctor-problems",
      "words": [
        "The bridge isn't running.",
        "Run `switchboard up`.",
        "Chats are found by polling, so new chats can take a few seconds to appear.",
        "Switchboard 0.3.0, installed in ~/bin.",
        "Configuration and key are private and valid.",
        "The server knows this machine as laptop (owner k).",
        "1 problem to fix, and 1 warning."
      ],
      "display": [
        {
          "kind": "item",
          "text": "The bridge isn't running.",
          "tone": "error",
          "fix": "Run `switchboard up`."
        },
        {
          "kind": "item",
          "text": "Chats are found by polling, so new chats can take a few seconds to appear.",
          "tone": "warning",
          "space": true
        },
        {
          "kind": "item",
          "text": "Switchboard 0.3.0, installed in ~/bin.",
          "tone": "success",
          "space": true
        },
        {
          "kind": "item",
          "text": "Configuration and key are private and valid.",
          "tone": "success"
        },
        {
          "kind": "item",
          "text": "The server knows this machine as laptop (owner k).",
          "tone": "success"
        },
        {
          "kind": "text",
          "space": true,
          "strong": true,
          "text": "1 problem to fix, and 1 warning."
        }
      ]
    },
    {
      "name": "doctor-all-passed",
      "words": [
        "Switchboard 0.3.0, installed in ~/bin.",
        "Configuration and key are private and valid.",
        "The bridge is running and connected.",
        "The server knows this machine as laptop (owner k).",
        "All checks passed."
      ],
      "display": [
        {
          "kind": "item",
          "text": "Switchboard 0.3.0, installed in ~/bin.",
          "tone": "success"
        },
        {
          "kind": "item",
          "text": "Configuration and key are private and valid.",
          "tone": "success"
        },
        {
          "kind": "item",
          "text": "The bridge is running and connected.",
          "tone": "success"
        },
        {
          "kind": "item",
          "text": "The server knows this machine as laptop (owner k).",
          "tone": "success"
        },
        {
          "kind": "text",
          "space": true,
          "strong": true,
          "text": "All checks passed."
        }
      ]
    },
    {
      "name": "doctor-down",
      "words": [
        "Down. Run `switchboard up` to reconnect.",
        "Switchboard 0.3.0, installed in ~/bin.",
        "Configuration and key are private and valid.",
        "The server knows this machine as laptop (owner k).",
        "All checks passed."
      ],
      "display": [
        {
          "kind": "item",
          "text": "Down. Run `switchboard up` to reconnect."
        },
        {
          "kind": "item",
          "text": "Switchboard 0.3.0, installed in ~/bin.",
          "tone": "success",
          "space": true
        },
        {
          "kind": "item",
          "text": "Configuration and key are private and valid.",
          "tone": "success"
        },
        {
          "kind": "item",
          "text": "The server knows this machine as laptop (owner k).",
          "tone": "success"
        },
        {
          "kind": "text",
          "space": true,
          "strong": true,
          "text": "All checks passed."
        }
      ]
    },
    {
      "name": "join",
      "words": [
        "Joined as solver.",
        "Reachable by 4 chats on 2 other machines."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Joined as solver.",
          "tone": "success"
        },
        {
          "kind": "text",
          "text": "Reachable by 4 chats on 2 other machines."
        }
      ]
    },
    {
      "name": "join-alone",
      "words": [
        "Joined as solver.",
        "No other machine has a joined chat right now."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Joined as solver.",
          "tone": "success"
        },
        {
          "kind": "text",
          "text": "No other machine has a joined chat right now."
        }
      ]
    },
    {
      "name": "join-not-live",
      "words": [
        "Joined as solver, but not live yet.",
        "The bridge is not running (switchboard up).",
        "If it stays this way, run",
        "switchboard doctor"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Joined as solver, but not live yet.",
          "tone": "warning"
        },
        {
          "kind": "text",
          "text": "The bridge is not running (switchboard up)."
        },
        {
          "kind": "next",
          "space": true,
          "text": "If it stays this way, run",
          "command": "switchboard doctor"
        }
      ]
    },
    {
      "name": "join-renamed",
      "words": [
        "Joined as second, but other machines still see it as first.",
        "The bridge has not published the new name within 10 seconds.",
        "If it stays this way, run",
        "switchboard doctor"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Joined as second, but other machines still see it as first.",
          "tone": "warning"
        },
        {
          "kind": "text",
          "text": "The bridge has not published the new name within 10 seconds."
        },
        {
          "kind": "next",
          "space": true,
          "text": "If it stays this way, run",
          "command": "switchboard doctor"
        }
      ]
    },
    {
      "name": "leave",
      "words": [
        "This chat left.",
        "Other machines can no longer see it."
      ],
      "display": [
        {
          "kind": "title",
          "text": "This chat left.",
          "tone": "success"
        },
        {
          "kind": "text",
          "text": "Other machines can no longer see it."
        }
      ]
    },
    {
      "name": "leave-unchanged",
      "words": [
        "This chat wasn't joined; nothing changed."
      ],
      "display": [
        {
          "kind": "title",
          "text": "This chat wasn't joined; nothing changed."
        }
      ]
    },
    {
      "name": "leave-unconfirmed",
      "words": [
        "This chat left, but the bridge hasn't confirmed it yet.",
        "The bridge is not running (switchboard up).",
        "If it stays this way, run",
        "switchboard doctor"
      ],
      "display": [
        {
          "kind": "title",
          "text": "This chat left, but the bridge hasn't confirmed it yet.",
          "tone": "warning"
        },
        {
          "kind": "text",
          "text": "The bridge is not running (switchboard up)."
        },
        {
          "kind": "next",
          "space": true,
          "text": "If it stays this way, run",
          "command": "switchboard doctor"
        }
      ]
    },
    {
      "name": "ping",
      "words": [
        "worker/research replied in 182 ms."
      ],
      "display": [
        {
          "kind": "title",
          "text": "worker/research replied in 182 ms.",
          "tone": "success"
        }
      ]
    },
    {
      "name": "ping-failed",
      "words": [
        "No reply from worker/nothing.",
        "No joined chat on another machine matches worker/nothing.",
        "run `switchboard chats --remote` to see them."
      ],
      "display": [
        {
          "kind": "title",
          "text": "No reply from worker/nothing.",
          "tone": "error"
        },
        {
          "kind": "item",
          "text": "No joined chat on another machine matches worker/nothing.",
          "fix": "run `switchboard chats --remote` to see them."
        }
      ]
    },
    {
      "name": "up",
      "words": [
        "Up. Connected to sb.example.com as laptop."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Up. Connected to sb.example.com as laptop.",
          "tone": "success"
        }
      ]
    },
    {
      "name": "up-not-connected",
      "words": [
        "Up, but not connected to sb.example.com yet.",
        "To see why, run",
        "switchboard doctor"
      ],
      "display": [
        {
          "kind": "title",
          "text": "Up, but not connected to sb.example.com yet.",
          "tone": "warning"
        },
        {
          "kind": "next",
          "space": true,
          "text": "To see why, run",
          "command": "switchboard doctor"
        }
      ]
    },
    {
      "name": "down",
      "words": [
        "Down. This machine's chats are offline until you run `switchboard up`.",
        "It stays down after a restart. Its setup and joined chats are kept."
      ],
      "display": [
        {
          "kind": "title",
          "text": "Down. This machine's chats are offline until you run `switchboard up`.",
          "tone": "success"
        },
        {
          "kind": "text",
          "text": "It stays down after a restart. Its setup and joined chats are kept."
        }
      ]
    },
    {
      "name": "help",
      "words": [
        "/switchboard [command]",
        "Command",
        "What it does",
        "status",
        "Whether this machine is connected, and this chat",
        "chats",
        "List chats on this machine and others",
        "join [name]",
        "Join this chat, so chats on other machines can message it",
        "leave",
        "Leave this chat; other machines can no longer see it",
        "ping <chat>",
        "Check that a chat on another machine is reachable",
        "up",
        "Bring this machine back online",
        "down",
        "Take this machine offline until you bring it back up",
        "doctor",
        "Check everything, with the fix for each problem",
        "help",
        "Show these commands",
        "Examples",
        "/switchboard join release-planner",
        "/switchboard chats --remote research",
        "/switchboard chats --details project-name",
        "/switchboard ping laptop/research",
        "Chats takes --local or --remote, --details, and search words that match names, IDs, machines, projects, repositories, and branches.",
        "A joined chat leaves after 7 days without work or messages. Closing or resuming a chat does not count.",
        "To add a machine, make an invite on the server with `sudo switchboard server invite`, then run `switchboard init` on the new machine."
      ],
      "display": [
        {
          "kind": "title",
          "text": "/switchboard [command]"
        },
        {
          "kind": "table",
          "space": true,
          "columns": [
            "Command",
            "What it does"
          ],
          "rows": [
            {
              "cells": [
                "status",
                "Whether this machine is connected, and this chat"
              ]
            },
            {
              "cells": [
                "chats",
                "List chats on this machine and others"
              ]
            },
            {
              "cells": [
                "join [name]",
                "Join this chat, so chats on other machines can message it"
              ]
            },
            {
              "cells": [
                "leave",
                "Leave this chat; other machines can no longer see it"
              ]
            },
            {
              "cells": [
                "ping <chat>",
                "Check that a chat on another machine is reachable"
              ]
            },
            {
              "cells": [
                "up",
                "Bring this machine back online"
              ]
            },
            {
              "cells": [
                "down",
                "Take this machine offline until you bring it back up"
              ]
            },
            {
              "cells": [
                "doctor",
                "Check everything, with the fix for each problem"
              ]
            },
            {
              "cells": [
                "help",
                "Show these commands"
              ]
            }
          ]
        },
        {
          "kind": "heading",
          "text": "Examples",
          "space": true
        },
        {
          "kind": "code",
          "text": "/switchboard join release-planner"
        },
        {
          "kind": "code",
          "text": "/switchboard chats --remote research"
        },
        {
          "kind": "code",
          "text": "/switchboard chats --details project-name"
        },
        {
          "kind": "code",
          "text": "/switchboard ping laptop/research"
        },
        {
          "kind": "text",
          "text": "Chats takes --local or --remote, --details, and search words that match names, IDs, machines, projects, repositories, and branches.",
          "space": true
        },
        {
          "kind": "text",
          "text": "A joined chat leaves after 7 days without work or messages. Closing or resuming a chat does not count."
        },
        {
          "kind": "text",
          "text": "To add a machine, make an invite on the server with `sudo switchboard server invite`, then run `switchboard init` on the new machine."
        }
      ]
    }
  ]
}
