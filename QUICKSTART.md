# Quick Start

## Prerequisites

- Docker
- The ability to run shell scripts
- git (only to clone the repository - alternatively use the "download zip" link on github)

Clanky has not been tested on Windows.

## Installation

- Clone the repository: `git clone https://github.com/kfeldmann/clanky.git && cd clanky`
- Build: `./build-linux-musl && ./build-runner`
- Make a symbolic link to make the runner script available everywhere (this assumes ~/bin/ is in your PATH): `ln -s $(pwd)/run ~/bin/clanky`

## Authentication

If your provider authentication uses an API key, set that as an environment variable:

- `DEEPINFRA_API_KEY` (or `DEEPINFRA_TOKEN`), optional `DEEPINFRA_URL`
- `LITELLM_API_KEY`, optional `LITELLM_BASE_URL`
- `ANTHROPIC_API_KEY`, optional `ANTHROPIC_BASE_URL`

With Anthropic, you can authenticate via OAuth if your Anthropic account supports it (for example, enterprise accounts):

- `./anthropic-login` - This command will print a URL and wait for you to paste the login code. Paste the provided URL into a web browser and follow the instructions to sign in and authorize clanky's access. Finally, copy the provided code and paste it on the command-line to complete the login.
- This is another script you may want to link into your PATH: `ln -s $(pwd)/anthropic-login ~/bin/anthropic-login`

## Usage

Change-directory into a project you want to work on, then run `clanky`

The interface consists of a status line and an input line:

    deepinfra · zai-org/GLM-5.3-Flash                   $0.0000
     ❯ 

The status line shows the current provider, the current model, and then the session cost on the right.

The default provider, if you don't specify one, is deepinfra. The fact that we see the session cost on the right side indicates that credentials for deepinfra were found in the environment and clanky has downloaded the pricing for the deepinfra provider plugin's default model. If no credentials were provided, you'd see a placeholder of command hints instead of the session cost.

You can see available slash commands by entering `/`

- Use `/provider` to choose a different provider.
- Use `/model` to choose a different model.
- Use `/thinking` to set a thinking value.

To enter a prompt, just start typing in the input area. Hit `enter` to submit your prompt.

While a turn is active, a **green dot** will show to the left of the ❯ symbol. When the dot goes away, the LLM's turn is done.

That's all you need to get going. Below are some optional tips for digging deeper.

## Tips

### Configuration Scopes

You can set the available settings at different scopes. More specific scopes override more general scopes:

    slash command > CLI > project > user > defaults

For example, you can set a model in `~/.clanky/settings.toml` (the user scope). If a model is not set anywhere else, that model will be used. If you then set a different model at the project scope (`<project-directory>/.clanky/settings.toml`), then that model will be used when running clanky in that project directory. Outside of that project, the user-level setting will be used. Even if a project-level model is set, you can override it with `--model` on the command-line when launching clanky. And finally, you can switch models inside a clanky session using `/model` which overrides all other scopes.

### System Prompt

There is no default system prompt. The system prompt is assembled from the following, from both user and project scopes in this order:

- `.clanky/SYSTEM.md`
- `.clanky/AGENTS.md`
- `.clanky/skills/<name>.md` and `.clanky/skills/<name>/SKILL.md`

Skills are "lazy-loaded". Only the name, description, and filepath are appended to the system prompt.

### Editing

You have rudimentary `readline` navigation keys available: ctl+a, ctl+e, and home/end, as well as the arrow keys. You can clear the entry using ctl+u. You can input a newline by pressing ctl+j. If the cursor is at the end of the input, ctl+e will launch the editor specified in your EDITOR environment variable (falling through to `vi` if nothing is set). The external editor is recommended for long or multi-line inputs. You can paste multi-line text into the input area, but beware that if you are running `screen`, there is a limitation in `screen` which will cause the pasted text to be split on newlines, sending only the first line of your prompt to the LLM - very annoying.

### Subagents

To use subagents, copy the `parallel-subagents` skill from `contrib/skills/` into your user or project level `.clanky/skills/` directory. This will teach the LLM how to run subagents. Explicitly mention the skill in your prompt to ensure it is read. For example, "Use the parallel-subagents skill to launch 4 subagents to do the following..."

### Additional Documentation

For more details, please read README.md and the other documentation in the repository.
