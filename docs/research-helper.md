# The research helper

[`research-helper`](../examples/research-helper/src/lib.rs) takes a topic, looks it up, asks an AI model about it and keeps notes. It's small, but it uses everything an agent can ask for, so this page goes through what each step looks like from your side.

Read [Writing an agent](writing-an-agent.md) first, which covers building and installing one. To use a real model, you need an API key, and it's yours to pay for.

## Setting it up

Build it, package it and install it:

```bash
cargo build --release -p habitat-research-helper --target wasm32-unknown-unknown
```

```bash
wasm-tools component new target/wasm32-unknown-unknown/release/habitat_research_helper.wasm -o research-helper.wasm
```

```bash
target/release/habitat pack examples/research-helper/agent.toml research-helper.wasm -o research-helper.habitat
```

```bash
target/release/habitat install research-helper.habitat
```

Add your API key. It goes in your system's keystore, and the agent never gets to see it:

```bash
target/release/habitat secret add anthropic --host api.anthropic.com --header x-api-key
```

## What its manifest asks for

```toml
[requests]
network = ["en.wikipedia.org"]
models = ["default"]
files = "ask"
```

**This doesn't give it anything.** It only means that the agent is allowed to ask you. Anything it isn't listing, it can't even ask for.

## A run

```bash
target/release/habitat run research-helper "research Honeybee"
```

It asks to reach Wikipedia:

```text
Agent "research-helper" is asking to reach the network host en.wikipedia.org.
The agent's own claim, which may not be true: to look up what you asked about
Allow this? [y] once, [r] for this run, [a] until revoked, [N] no:
```

The first line is Habitat's own, and the agent can't change it. The second is what the agent says, so it's labelled as a claim.

- **`y`** allows it once, **`r`** for this run, and **`a`** until you remove it with `habitat revoke`.
- **Anything else is no,** and so is no answer within 2 minutes.

Then it asks to use the model in the same way, and shows you the answer:

```text
[research-helper] Honeybees are ...
completed (run 01a1...)
```

## If you say no

Saying no doesn't crash anything. If you refuse Wikipedia, it says so, and carries on without it:

```text
[research-helper] Looking it up was refused: the user said no. I'll carry on without it.
```

If you refuse the model, it has nothing more to do, so the run ends with a reason, and exit code 3:

```text
error: failed, and the agent said: Using the model was refused: the user said no (run 01a1...)
```

## Its notes

Each run starts from nothing, and only its files carry over. It saves its notes before showing you the answer, so that they're kept even if it's stopped there:

```bash
target/release/habitat run research-helper "notes"
```

## Using one of your files

```bash
target/release/habitat run research-helper "research-file Honeybee"
```

It asks you to choose a file, and you type its path. It gets that file's name and contents, and never its folder.

**From then on, anywhere it wants to reach needs a fresh approval, even if you've already said `a`.** The prompt tells you why:

```text
Agent "research-helper" is asking to use the AI model default.
It has already read a private file in this run, so it could send that there.
The agent's own claim, which may not be true: to answer what you asked
```

This is the data-leak rule. It helps, but it isn't a guarantee, as [the threat model](threat-model.md#known-weak-spots) explains.

## What the log shows

```bash
target/release/habitat log research-helper
```

```text
1 ... asked for network host en.wikipedia.org
2 ... allowed network host en.wikipedia.org once
3 ... asked for network host en.wikipedia.org
4 ... GET to network host en.wikipedia.org gave 200
5 ... asked for AI model default
6 ... allowed AI model default once
7 ... asked for AI model default
8 ... AI model default used 412 tokens in and 96 out
9 ... showed the user a message
10 ... run ended: completed
```

It holds what was asked for, what was decided and how many tokens were used. It doesn't hold what was asked of the model, the file's name or any URL's path, as those can be private. And it never holds your API key.

To check that it hasn't been changed:

```bash
target/release/habitat log research-helper --verify
```
