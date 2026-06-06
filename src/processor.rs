use anyhow::{anyhow, Result};
use std::{collections::{BTreeMap, HashMap}, error::Error, fs, io::Write, path};
use tracing::{debug, info, warn};

use crate::{app_project::*, Args, Config, Metadata, TemplateContext};

pub struct ProjectProcessor {
    input_path: path::PathBuf,
    output_path: path::PathBuf,
    application_template_name: String,
    config: Config,
    targets: BTreeMap<String, BTreeMap<String, ArgoCDProject>>,
    tera: tera::Tera,
}

pub struct ArgoCDProject {
    project: AppProject,
    applications: Vec<String>,
}

impl ProjectProcessor {
    pub fn new(args: Args) -> Result<ProjectProcessor> {
        let input_path = match args.input_path {
            Some(p) => std::path::PathBuf::from(p),
            None => std::path::PathBuf::from("."),
        };

        let output_path = match args.output_path {
            Some(v) => std::path::PathBuf::from(v),
            None => tempdir::TempDir::new("argocd-preprocessor")?
                .path()
                .to_path_buf(),
        };
        let input_path = input_path.canonicalize()?;
        // make the output directory before calling canonicalize to avoid the not exist erro
        fs::create_dir_all(&output_path)?;
        let output_path = output_path.canonicalize()?;

        info!(input_path=?input_path, output_path=?output_path, "resolved input and output paths");
        let config = read_config(&input_path)?;

        let template_path = input_path.join(&config.application_template);
        let template_name = template_path.strip_prefix(&input_path);
        let template_name = template_name?.display().to_string();

        let tera_template_path = input_path.join("**/*.tera").to_string_lossy().to_string();
        let mut tera = tera::Tera::new(&tera_template_path)?;
        tera.register_filter("yaml_encode", yaml_encode_filter);
        tera.register_filter("nindent", nindent_filter);

        return Ok(ProjectProcessor {
            input_path,
            output_path,
            application_template_name: template_name,
            targets: BTreeMap::new(),
            config,
            tera,
        });
    }

    pub fn process(&mut self) -> Result<()> {
        let mut vars_by_target: HashMap<String, serde_json::Value> = HashMap::new();

        for target in &self.config.targets {
            let target_dir = self.output_path.join(&target.name);
            if target_dir.exists() {
                fs::remove_dir_all(&target_dir)?;
            }
            fs::create_dir_all(target_dir)?;

            self.targets.insert(target.name.clone(), BTreeMap::new());

            let mut merged_vars = self
                .config
                .vars
                .clone()
                .unwrap_or_else(default_serde_object);
            merge(
                &mut merged_vars,
                target.vars.clone().unwrap_or_else(default_serde_object),
            );
            vars_by_target.insert(target.name.clone(), merged_vars);
        }

        for metadata_file in glob::glob(self.input_path.join("**/metadata.toml").to_str().unwrap())?
        {
            let metadata_file = metadata_file
                .or_else(|e| Err(anyhow!("failed to glob for metadata.toml files: {}", e)))?;

            info!(file = ?metadata_file, "processing file");
            let metadata = read_metadata(metadata_file.as_path())?;

            let app_dir = metadata_file.parent().ok_or(anyhow!(
                "unable to find parent associated with metadata.toml file ({:?})",
                metadata_file
            ))?;

            for target in metadata.targets.iter() {
                if !self.targets.contains_key(&target.name) {
                    warn!(target=target.name, path=?app_dir, "skipping unknown target");
                    continue;
                }

                let app_context =
                    self.template_context_for_dir(app_dir, &target.name, &metadata)?;

                self.create_or_update_app_project_for_dir(&target.name, &metadata, &app_context);
                let argo_application = self.generate_argo_application_for_dir(
                    &metadata.application_options,
                    &app_context,
                )?;

                let target_project = self
                    .targets
                    .get_mut(&target.name)
                    .unwrap()
                    .get_mut(&app_context.project)
                    .unwrap();

                target_project.applications.push(argo_application);

                let out_folder_path = self.output_path.join(&app_context.path);
                // debug!(from_path=?self.input_path, to_path=?out_folder_path, "copying");

                let mut target_vars = vars_by_target.get(&target.name).unwrap().clone(); // unwrap since the value should always be there due to the above for loop
                merge(
                    &mut target_vars,
                    target.vars.clone().unwrap_or_else(default_serde_object),
                );

                self.copy_and_template_folder(
                    &target_vars,
                    &app_dir.to_path_buf(),
                    &out_folder_path,
                )?;

                self.write_bargo_values(&target_vars, &app_context, &out_folder_path)?;

                match metadata.script.as_ref() {
                    Some(script) => {
                        let output = std::process::Command::new("bash")
                            .arg("-c")
                            .arg(script)
                            .env("in", app_dir)
                            .env("out", out_folder_path)
                            .output()?;
                        info!(output=?output, "script output");
                        if !output.status.success() {
                            return Err(anyhow!(
                                "script exited with a non zero: {:?}",
                                output.stdout
                            ));
                        }
                    }
                    None => (),
                };
            }
        }

        for (target_name, target) in self.targets.iter() {
            let config_dir = self.output_path.join(target_name).join("argocd-config");
            fs::create_dir_all(&config_dir)?;

            // write application file for argo_cd

            let mut file = fs::File::create(config_dir.join("argocd-config.yaml"))?;
            let app = self.generate_argo_application_for_dir(
                &self.config.argocd_config_application_options,
                &TemplateContext {
                    namespace: self.config.argocd_namespace.clone(),
                    project: "default".to_string(),
                    app_name: "argocd-config".to_string(),
                    normalized_project: "default".to_string(),
                    normalized_app_name: "argocd-config".to_string(),
                    path: format!("{}/argocd-config", target_name),
                    target_name: target_name.to_string(),
                },
            )?;
            file.write_all(app.as_bytes())?;
            // write application files for all folders
            for (project_name, project) in target.iter() {
                let mut file =
                    fs::File::create(config_dir.join(format!("{:}.yaml", project_name)))?;

                file.write_all(serde_yaml::to_string(&project.project)?.as_bytes())?;
                let mut sorted_apps = project.applications.clone();
                sorted_apps.sort();
                for app in sorted_apps.iter() {
                    file.write_all(b"\n---\n")?;
                    file.write_all(app.as_bytes())?;
                }
            }
        }

        return Ok(());
    }

    fn generate_argo_application_for_dir(
        &self,
        application_options: &Option<serde_json::Value>,
        app_context: &TemplateContext,
    ) -> Result<String> {
        let mut template_context = self
            .config
            .default_application_options
            .clone()
            .unwrap_or_else(default_serde_object);

        merge(
            &mut template_context,
            application_options
                .clone()
                .unwrap_or_else(default_serde_object),
        );

        merge(&mut template_context, serde_json::to_value(app_context)?);

        return self.render_template(&self.application_template_name, template_context);
    }

    fn create_or_update_app_project_for_dir(
        &mut self,
        target_name: &str,
        metadata: &Metadata,
        app_context: &TemplateContext,
    ) {
        let project = self
            .targets
            .get_mut(target_name)
            .unwrap()
            .entry(app_context.normalized_project.clone())
            .or_insert(ArgoCDProject {
                project: AppProject::new(
                    app_context.normalized_project.clone(),
                    self.config.argocd_namespace.clone(),
                ),
                applications: Vec::new(),
            });
        // set all the array like things are using hashsets we can ruthleslsly add everything and
        // duplicates will get auto dedupped
        project
            .project
            .spec
            .source_repos
            .insert(self.config.argocd_source_repo.clone());

        project
            .project
            .spec
            .destinations
            .insert(AppProjectDestination {
                name: "in-cluster".to_string(),
                namespace: app_context.namespace.clone(),
                server: "https://kubernetes.devault.svc".to_string(),
            });

        project.project.spec.cluster_resource_whitelist.insert(
            AppProjectClusterResourceWhitelist {
                group: "".to_string(),
                kind: "Namespace".to_string(),
            },
        );

        match metadata.project_options.as_ref() {
            Some(options) => {
                match options.additional_namespaces.as_ref() {
                    Some(additional_namespaces) => {
                        for namespace in additional_namespaces.iter() {
                            project
                                .project
                                .spec
                                .destinations
                                .insert(AppProjectDestination {
                                    name: "in-cluster".to_string(),
                                    namespace: namespace.to_string(),
                                    server: "https://kubernetes.devault.svc".to_string(),
                                });
                        }
                    }
                    None => (),
                }

                match options.cluster_resource_whitelist.as_ref() {
                    Some(cluster_resource_whitelist) => {
                        for allow_list_item in cluster_resource_whitelist.iter() {
                            project
                                .project
                                .spec
                                .cluster_resource_whitelist
                                .insert(allow_list_item.clone());
                        }
                    }
                    None => (),
                }
            }
            None => (),
        }
    }

    fn template_context_for_dir(
        &self,
        app_dir: &path::Path,
        target_name: &str,
        metadata: &Metadata,
    ) -> Result<crate::TemplateContext> {
        let project = app_dir
            .parent()
            .ok_or(anyhow!(
                "unable to determine project name from folder structure for {:?}",
                app_dir
            ))?
            .file_name()
            .ok_or(anyhow!(
                "unable to determine project name from folder structure for {:?}",
                app_dir
            ))?
            .to_string_lossy()
            .to_string();

        let app_name = app_dir
            .file_name()
            .ok_or(anyhow!(
                "unable to determine app name from folder structure for {:?}",
                app_dir
            ))?
            .to_string_lossy()
            .to_string();

        let out_path = path::PathBuf::new()
            .join(&target_name)
            .join(&project)
            .join(&app_name);

        return Ok(TemplateContext {
            namespace: metadata
                .namespace
                .clone()
                .unwrap_or(sanitize_name(&project)),
            normalized_project: sanitize_name(&project),
            normalized_app_name: sanitize_name(&app_name),
            project,
            app_name,
            path: out_path.display().to_string(),
            target_name: target_name.to_string(),
        });
    }

    fn render_template(
        &self,
        template_name: &str,
        template_context: serde_json::Value,
    ) -> Result<String> {
        self.tera
            .render(template_name, &tera::Context::from_value(template_context)?)
            .map_err(|e| match e.source() {
                Some(err_source) => anyhow!("{:#}", err_source),
                None => anyhow!("{}", e),
            })
    }

    fn copy_and_template_folder(
        &self,
        tera_context: &serde_json::Value,
        from_dir: &path::PathBuf,
        to_dir: &path::PathBuf,
    ) -> Result<()> {
        // info!(from=?from_dir, to=?to_dir, "copying!");
        for f in fs::read_dir(from_dir)? {
            let entry = f?;
            let path = entry.path();
            if path.is_dir() {
                self.copy_and_template_folder(
                    &tera_context.clone(),
                    &path,
                    &to_dir.join(&entry.file_name()),
                )?;
                continue;
            }
            let mut to_path = to_dir.join(&entry.file_name());
            fs::create_dir_all(to_path.parent().unwrap())?;

            if path.extension().unwrap_or_default() == "tera" {
                info!(vars=?tera_context, to_path=?to_path, "templating file");
                let tera_template_name = path.strip_prefix(&self.input_path)?;
                let contents = self.render_template(
                    &tera_template_name.display().to_string(),
                    tera_context.clone(),
                )?;
                to_path.set_extension("");
                fs::write(to_path, contents)?;
                continue;
            }

            debug!(from_path=?path, to_path=?to_path, "copying file");
            fs::copy(path, to_path)?;
        }
        return Ok(());
    }

    fn write_bargo_values(
        &self,
        tera_context: &serde_json::Value,
        template_context: &TemplateContext,
        to_dir: &path::PathBuf,
    ) -> Result<()> {
        if !to_dir.join("files/Chart.yaml").exists() {
            info!(
                ?to_dir,
                "not writing bargo values file as didn't find Chart.yaml file in files subdir"
            );
            return Ok(());
        }

        let mut bargo_vars = serde_json::Map::new();
        bargo_vars.insert("vars".to_string(), serde_json::to_value(tera_context)?);
        bargo_vars.insert(
            "metadata".to_string(),
            serde_json::to_value(template_context)?,
        );

        let mut bargo_values_map = serde_json::Map::new();
        bargo_values_map.insert("bargo".to_string(), serde_json::Value::Object(bargo_vars));
        // put values under global namespace so that all subcharts have access to them
        let mut values_map = serde_json::Map::new();
        values_map.insert(
            "global".to_string(),
            serde_json::Value::Object(bargo_values_map),
        );
        let values = serde_json::Value::Object(values_map);

        let s_values = yaml_encode(&values)?;
        let mut file = fs::File::create(to_dir.join("files/bargo_values.yaml"))?;
        file.write_all(s_values.as_bytes())?;
        file.flush()?;

        Ok(())
    }
}

// based on https://github.com/argoproj/applicationset/blob/de10506d8ff81970567381ef3f4dae4b76f50220/pkg/generators/cluster.go#L172
// santize the name in accordance with the below rules
// 1. contain no more than 253 characters
// 2. contain only lowercase alphanumeric characters, '-' or '.'
// 3. start and end with an alphanumeric character
fn sanitize_name(name: &str) -> String {
    let invalid_dns_name_chars = regex::Regex::new(r"[^-a-z0-9.]").unwrap();
    let max_dns_name_length = 253;

    let name = name.to_lowercase();
    let name = invalid_dns_name_chars.replace_all(&name, "-").to_string();
    let name = if name.len() > max_dns_name_length {
        name[..max_dns_name_length].to_string()
    } else {
        name
    };

    return name.trim_matches('-').to_string();
}

fn read_config(input_path: &path::Path) -> Result<Config> {
    let config_file_path = input_path.join("bargo.toml");
    let config = fs::read(&config_file_path)
        .map_err(|e| anyhow!("failed to parse config file {:?}: {}", config_file_path, e))?;
    let config = toml::from_slice(&config)
        .map_err(|e| anyhow!("failed to parse config file {:?}: {}", config_file_path, e))?;

    info!(config_file_path=?config_file_path, config=?config, "loaded config");
    return Ok(config);
}

fn default_serde_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn read_metadata(metadata_file: &path::Path) -> Result<crate::Metadata> {
    let config = fs::read(&metadata_file)
        .map_err(|e| anyhow!("failed to parse config file {:?}: {}", metadata_file, e))?;
    let config = toml::from_slice(&config)
        .map_err(|e| anyhow!("failed to parse config file {:?}: {}", metadata_file, e))?;

    info!(file=?metadata_file, config=?config, "loaded metadata file");
    return Ok(config);
}

// from: https://stackoverflow.com/questions/47070876/how-can-i-merge-two-json-objects-with-rust
fn merge(a: &mut serde_json::Value, b: serde_json::Value) {
    if let serde_json::Value::Object(a) = a {
        if let serde_json::Value::Object(b) = b {
            for (k, v) in b {
                if v.is_null() {
                    a.remove(&k);
                } else {
                    merge(a.entry(k).or_insert(serde_json::Value::Null), v);
                }
            }

            return;
        }
    }

    *a = b;
}

// Encodes a value of any type into yaml
fn yaml_encode_filter(
    value: &serde_json::Value,
    _args: &HashMap<String, serde_json::Value>,
) -> tera::Result<serde_json::Value> {
    return yaml_encode(value)
        // serde_yaml::to_string(&value)
        //     .map(|s| s.trim().to_string())
        .map(serde_json::Value::String)
        .map_err(|e| tera::Error::from(format!("{e}")));
}

fn yaml_encode(value: &serde_json::Value) -> Result<String> {
    Ok(serde_yaml::to_string(&value).map(|s| s.trim().to_string())?)
}

// Indents each line of a string
fn nindent_filter(
    value: &serde_json::Value,
    args: &HashMap<String, serde_json::Value>,
) -> tera::Result<serde_json::Value> {
    let s = tera::try_get_value!("nindent", "value", String, value);
    let spaces = match args.get("spaces") {
        Some(spaces) => tera::try_get_value!("nindent", "spaces", usize, spaces),
        None => {
            return Err(tera::Error::msg(
                "Filter `nindent` expected an arg called `spaces`",
            ))
        }
    };

    let indent = " ".repeat(spaces);
    let indent = format!("\n{}", indent);

    let s = format!("\n{}", s);
    let s = s.replace("\n", &indent);
    return Ok(serde_json::Value::String(s));
}
