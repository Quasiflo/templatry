# Templatry

Write Once, Use Everywhere!

Templatry is a tool to template & distribute configuration files across your team's repositories. Currently, many files like linter & formatter settings, renovate manifest, gitignores and so much more are copy-pasted across numerous repositories, often with slight tweaks for each repo. Templatry lets you standardize your templates in a single source repository, and only commit overrides to your projects. Everything gets generated deterministically, updates when you bump your template version, and still gives you full per-project control to deviate how you need, without writing everything else from scratch every time.

## Details

There's two locations where Templatry will run. In the template source repository (where all the template configs are stored) - here it's only job is to check the templatry.toml is valid. And in the project repository, where it will:
- Retrieve template from specified location. Can be relative or absolute file path, github release, url etc. if download (not local path), gets cached in a user-wide folder for access across projects (location TBD, also discuss integrity checking in case modified between invocations). It's expected to be (unless local path) an archive containing a templatry.source.toml which specifies all the template files, merging strategy, etc.

Main jobs:

- Generate configuration files using template file, repository override files (must detect file changes and re-run also, and also re-run if templatary config file changes) and merge them using algorithm defined by template source config file.
- Back-propagate changes to generated files into template overrides (certain configs)
- Validate template repository setup

templatry.source.toml rough structure: (templatry.source.toml must be in root of archive)

```toml
[configs]
default_generated_dir # where to place generated configs by default. defaults to .config/generated
default_override_dir # where to look for local overriding configs by default. defaults to .config/

[templates]
whatever_config {
    template = /some/path.json
    override_file = whatever_config_2.json # Defaults to template file name
    override_dir = /somewhere/expected_repo_override_path/ # defaults to default_override_dir
    generated_file = whatever_resultant_config.json # Defaults to template file name
    generated_dir = .config/elsewhere/ # defaults to default_generated_dir
    strategy = append # how to merge template with override. for now, options are append (add override to end of file), json_merge, yaml_merge, toml_merge. We may want to discuss this more to incorporate deep vs shallow merge, etc or other options. Defaults to auto-detect on file extension.
    back_propagate = false # default false, if true, watches the generated file for changes too, and back propagates them into the template overrides file such that when generator is rerun with updated template, the output is how it was modified to be
    labels = { "javascript", "oxc" } # list of arbitrary labels to associate
}

[default]
include_labels # Mutually exclusive lists that either set up as default-disabled except these list
exclude_labels # Or default-enabled-except-these. If nether is defined, everything is on by default. Empty enabled list disables everything by default.
```

templatry.toml rough structure: (templatry.toml must be in .config/templatry.toml)

```toml
[source.my_source_arbitrary_name]
github = "some_gh_url"
ref = "some_tag"
enable_labels = # enable these labels on top of what source [default] sets up
disable_labels =  # disable these labels on top of what source [default] sets up
```

running templatry will generate configs, templatry --watch will continue watching files for changes to regenerate as needed.
