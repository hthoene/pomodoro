# Pomodoro

A plugin for [PaperPaw](https://paperpaw.org). Its folder is `plugin/`; `plugin/store/store.ini` says what the plugin store shows of it.

## Publishing

1. Make the repository public on GitHub and give it the topic `paperpaw-plugin` (About, the gear, Topics): the store finds it by that.
2. Write `description` in `plugin/plugin.ini`, choose the `group` and `tags` in `plugin/store/store.ini`, and add a screenshot to `plugin/store/` with what it shows under `[screenshots]`.
3. Make your author key once (`paperpaw-sign key ~/.paperpaw/author.key`), keep the file safe and out of every repository, and add its contents as the repository's secret `PAPERPAW_PLUGIN_KEY` (Settings, Secrets and variables, Actions). Build once (`cargo x4`) and commit `Cargo.lock`.
4. Check it as the store will: `paperpaw-dev check-publish` (or `paperpaw-sign check-publish`).
5. Tag the version of `plugin.ini` and push the tag: `git tag v0.1.0`, `git push --tags`. The workflow checks it again, then makes the release; within the hour the store lists `pomodoro` under Community plugins, unreviewed. The reviewed badge you can ask for later, in the store's form (From GitHub).
