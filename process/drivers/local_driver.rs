use blue_build_utils::{cmd, string_vec};
use log::trace;
use miette::bail;

use super::{opts::GenerateTagsOpts, CiDriver, Driver};

pub struct LocalDriver;

impl CiDriver for LocalDriver {
    fn on_default_branch() -> bool {
        trace!("LocalDriver::on_default_branch()");
        false
    }

    fn keyless_cert_identity() -> miette::Result<String> {
        trace!("LocalDriver::keyless_cert_identity()");
        bail!("Keyless not supported");
    }

    fn oidc_provider() -> miette::Result<String> {
        trace!("LocalDriver::oidc_provider()");
        bail!("Keyless not supported");
    }

    fn generate_tags(opts: &GenerateTagsOpts) -> miette::Result<Vec<String>> {
        trace!("LocalDriver::generate_tags({opts:?})");
        let os_version = Driver::get_os_version(opts.oci_ref)?;
        let timestamp = blue_build_utils::get_tag_timestamp();
        let short_sha = commit_sha();

        Ok(opts.alt_tags.as_ref().map_or_else(
            || {
                let mut tags = string_vec![
                    "latest",
                    &timestamp,
                    format!("{os_version}"),
                    format!("{timestamp}-{os_version}"),
                ];

                if let Some(short_sha) = &short_sha {
                    tags.push(format!("{short_sha}-{os_version}"));
                }

                tags
            },
            |alt_tags| {
                alt_tags
                    .iter()
                    .flat_map(|alt| {
                        let mut tags = string_vec![
                            &**alt,
                            format!("{alt}-{os_version}"),
                            format!("{timestamp}-{alt}-{os_version}"),
                        ];
                        if let Some(short_sha) = &short_sha {
                            tags.push(format!("{short_sha}-{alt}-{os_version}"));
                        }

                        tags
                    })
                    .collect()
            },
        ))
    }

    fn get_repo_url() -> miette::Result<String> {
        trace!("LocalDriver::get_repo_url()");
        Ok(String::new())
    }

    fn get_registry() -> miette::Result<String> {
        trace!("LocalDriver::get_registry()");
        Ok(String::from("localhost"))
    }
}

fn commit_sha() -> Option<String> {
    let output = cmd!("git", "rev-parse", "--short", "HEAD").output().ok()?;

    if output.status.success() {
        String::from_utf8(output.stdout)
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    }
}