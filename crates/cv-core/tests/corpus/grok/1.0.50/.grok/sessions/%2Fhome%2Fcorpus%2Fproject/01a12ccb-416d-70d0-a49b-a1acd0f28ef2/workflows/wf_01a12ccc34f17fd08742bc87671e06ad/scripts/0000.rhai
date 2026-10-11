let meta = #{
    name: "corpus-hello",
    description: "Two agents each reply with one word",
    phases: [ #{ title: "Say", detail: "Each agent replies with one word" } ],
};
phase("Say");
let a = agent("Reply with the single word ok. Do not use any tools.", #{ label: "say-ok", capability_mode: "read-only" });
let b = agent("Reply with the single word yes. Do not use any tools.", #{ label: "say-yes", capability_mode: "read-only" });
#{ ok: a.success, yes: b.success }
