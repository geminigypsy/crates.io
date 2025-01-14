use crate::background_jobs::Environment;
use crate::git::{Crate, Credentials};
use crate::models::Version;
use crate::schema::versions;
use chrono::Utc;
use std::fs::{self, OpenOptions};
use std::io::prelude::*;
use swirl::PerformError;

#[swirl::background_job]
pub fn add_crate(env: &Environment, krate: Crate) -> Result<(), PerformError> {
    use std::io::prelude::*;

    let repo = env.lock_index()?;
    let dst = repo.index_file(&krate.name);

    // Add the crate to its relevant file
    fs::create_dir_all(dst.parent().unwrap())?;
    let mut file = OpenOptions::new().append(true).create(true).open(&dst)?;
    serde_json::to_writer(&mut file, &krate)?;
    file.write_all(b"\n")?;

    let message: String = format!("Updating crate `{}#{}`", krate.name, krate.vers);

    repo.commit_and_push(&message, &repo.relative_index_file(&krate.name))
}

/// Yanks or unyanks a crate version. This requires finding the index
/// file, deserlialise the crate from JSON, change the yank boolean to
/// `true` or `false`, write all the lines back out, and commit and
/// push the changes.
#[swirl::background_job]
pub fn yank(
    conn: &PgConnection,
    env: &Environment,
    krate: String,
    version: Version,
    yanked: bool,
) -> Result<(), PerformError> {
    use diesel::prelude::*;

    let repo = env.lock_index()?;
    let dst = repo.index_file(&krate);

    conn.transaction(|| {
        let yanked_in_db: bool = versions::table
            .find(version.id)
            .select(versions::yanked)
            .for_update()
            .first(&*conn)?;

        if yanked_in_db == yanked {
            // The crate is alread in the state requested, nothing to do
            return Ok(());
        }

        let prev = fs::read_to_string(&dst)?;
        let new = prev
            .lines()
            .map(|line| {
                let mut git_crate = serde_json::from_str::<Crate>(line)
                    .map_err(|_| format!("couldn't decode: `{}`", line))?;
                if git_crate.name != krate || git_crate.vers != version.num {
                    return Ok(line.to_string());
                }
                git_crate.yanked = Some(yanked);
                Ok(serde_json::to_string(&git_crate)?)
            })
            .collect::<Result<Vec<_>, PerformError>>();
        let new = new?.join("\n") + "\n";
        fs::write(&dst, new.as_bytes())?;

        let message: String = format!(
            "{} crate `{}#{}`",
            if yanked { "Yanking" } else { "Unyanking" },
            krate,
            version.num
        );

        repo.commit_and_push(&message, &repo.relative_index_file(&krate))?;

        diesel::update(&version)
            .set(versions::yanked.eq(yanked))
            .execute(&*conn)?;

        Ok(())
    })
}

/// Collapse the index into a single commit, archiving the current history in a snapshot branch.
#[swirl::background_job]
pub fn squash_index(env: &Environment) -> Result<(), PerformError> {
    let repo = env.lock_index()?;
    println!("Squashing the index into a single commit.");

    let now = Utc::now().format("%Y-%m-%d");
    let original_head = repo.head_oid()?.to_string();
    let msg = format!("Collapse index into one commit\n\n\
        Previous HEAD was {}, now on the `snapshot-{}` branch\n\n\
        More information about this change can be found [online] and on [this issue].\n\n\
        [online]: https://internals.rust-lang.org/t/cargos-crate-index-upcoming-squash-into-one-commit/8440\n\
        [this issue]: https://github.com/rust-lang/crates-io-cargo-teams/issues/47", original_head, now);

    repo.squash_to_single_commit(&msg)?;

    // Shell out to git because libgit2 does not currently support push leases

    let key = match &repo.credentials {
        Credentials::Ssh { key } => key,
        Credentials::Http { .. } => {
            return Err(String::from("squash_index: Password auth not supported").into())
        }
        _ => return Err(String::from("squash_index: Could not determine credentials").into()),
    };

    // When running on production, ensure the file is created in tmpfs and not persisted to disk
    #[cfg(target_os = "linux")]
    let mut temp_key_file = tempfile::Builder::new().tempfile_in("/dev/shm")?;

    // For other platforms, default to std::env::tempdir()
    #[cfg(not(target_os = "linux"))]
    let mut temp_key_file = tempfile::Builder::new().tempfile()?;

    temp_key_file.write_all(key.as_bytes())?;

    let checkout_path = repo.checkout_path.path();
    let output = std::process::Command::new("git")
        .current_dir(checkout_path)
        .env(
            "GIT_SSH_COMMAND",
            format!(
                "ssh -o StrictHostKeyChecking=accept-new -i {}",
                temp_key_file.path().display()
            ),
        )
        .args(&[
            "push",
            // Both updates should succeed or fail together
            "--atomic",
            "origin",
            // Overwrite master, but only if it server matches the expected value
            &format!("--force-with-lease=refs/heads/master:{}", original_head),
            // The new squashed commit is pushed to master
            "HEAD:refs/heads/master",
            // The previous value of HEAD is pushed to a snapshot branch
            &format!("{}:refs/heads/snapshot-{}", original_head, now),
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = format!("Running git command failed with: {}", stderr);
        return Err(message.into());
    }

    println!("The index has been successfully squashed.");

    Ok(())
}
