//! Restore our changes without overwriting later changes by other owners.
use super::*;
use crate::platform::display::recovery as native;

fn same_mode(a: &SavedTarget, b: &SavedTarget) -> bool {
    (
        a.width,
        a.height,
        a.left,
        a.top,
        a.rotation,
        a.refresh_numerator,
        a.refresh_denominator,
        a.scan_line_ordering,
    ) == (
        b.width,
        b.height,
        b.left,
        b.top,
        b.rotation,
        b.refresh_numerator,
        b.refresh_denominator,
        b.scan_line_ordering,
    )
}

impl State {
    pub(super) fn restore_layout(
        &self,
        baseline: &SavedTopology,
        dpi: &[(String, u32)],
    ) -> Result<()> {
        let current = Topology::query(true)?.snapshot()?;
        let expected = self
            .journal
            .desired
            .as_ref()
            .or(self.journal.applied.as_ref());
        let mut restore = SavedTopology::default();
        for original in &baseline.targets {
            let now = current
                .targets
                .iter()
                .find(|t| t.identity == original.identity);
            let after =
                expected.and_then(|s| s.targets.iter().find(|t| t.identity == original.identity));
            match (now, after) {
                (Some(now), Some(after)) if !same_mode(now, after) => {
                    let mut keep = now.clone();
                    keep.source_group = format!("current/{}", keep.source_group);
                    restore.targets.push(keep);
                }
                (None, Some(_)) => (), // The user unplugged/disabled it after our change.
                _ => restore.targets.push(original.clone()),
            }
        }
        for now in &current.targets {
            if !baseline.targets.iter().any(|t| t.identity == now.identity)
                && !self
                    .journal
                    .owned
                    .iter()
                    .any(|o| o.identity.as_deref() == Some(now.identity.as_str()))
            {
                let mut keep = now.clone();
                keep.source_group = format!("current/{}", keep.source_group);
                restore.targets.push(keep);
            }
        }
        let unchanged = restore.targets.len() == current.targets.len()
            && restore.targets.iter().all(|a| {
                current
                    .targets
                    .iter()
                    .any(|b| a.identity == b.identity && same_mode(a, b))
            });
        if !restore.targets.is_empty() && !unchanged {
            let missing = restore.apply()?;
            if !missing.is_empty() {
                tracing::warn!(
                    targets = missing.len(),
                    "unavailable display targets omitted during recovery"
                );
            }
        }
        let targets = Topology::query(true)?.targets()?;
        for (identity, original) in dpi {
            let expected = self
                .journal
                .desired_dpi
                .iter()
                .chain(&self.journal.applied_dpi)
                .find(|(id, _)| id == identity)
                .map(|(_, v)| *v);
            if let Some(target) = targets.iter().find(|t| &t.identity == identity) {
                if target
                    .dpi
                    .as_ref()
                    .is_some_and(|d| Some(d.current) == expected || d.current == *original)
                {
                    target.set_dpi(*original, || true)?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn intend_layout(&mut self, layout: &SavedTopology) -> Result<()> {
        self.journal.desired = Some(layout.clone());
        self.save()
    }
    pub(super) fn intend_mode(&mut self, identity: &str, width: u32, height: u32) -> Result<()> {
        let mut layout = Topology::query(true)?.snapshot()?;
        let target = layout
            .targets
            .iter_mut()
            .find(|t| t.identity == identity)
            .context("显示目标已断开")?;
        target.width = width;
        target.height = height;
        self.intend_layout(&layout)
    }
    pub(super) fn intend_dpi(&mut self, identity: &str, dpi: u32) -> Result<()> {
        self.journal.desired_dpi.retain(|(id, _)| id != identity);
        self.journal.desired_dpi.push((identity.into(), dpi));
        self.save()
    }
}

pub(crate) fn watch(token: &str) -> Result<()> {
    let token = uuid::Uuid::parse_str(token)?.to_string();
    let path = store::root()?
        .join("recovery")
        .join(format!("{token}.json"));
    let journal: Journal = store::read(&path)?.context("显示恢复记录不存在")?;
    ensure!(journal.token == token, "显示恢复标识不匹配");
    let parent = native::parent(journal.owner_pid, journal.owner_birth)?;
    native::ready(&token)?;
    while !native::exited(&parent, 500)? {
        let Some(journal) = store::read::<Journal>(&path)? else {
            return Ok(());
        };
        if journal.retired {
            break;
        }
    }
    recover(&path)
}

fn recover(path: &std::path::Path) -> Result<()> {
    let _serial = native::Serial::acquire()?;
    let Some(journal) = store::read::<Journal>(path)? else {
        return Ok(());
    };
    ensure!(
        journal.retired || !native::owner_alive(journal.owner_pid, journal.owner_birth)?,
        "显示拥有者仍在运行"
    );
    let mut state = State {
        journal,
        path: path.into(),
        preferences: Preference::default(),
        has_saved_preferences: false,
        preference_path: None,
        driver: None,
        render_adapter: None,
        dirty: true,
        restoring: true,
    };
    state.cleanup()
}

pub(crate) fn recover_abandoned() -> Result<()> {
    let directory = store::root()?.join("recovery");
    if !directory.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(journal) = store::read::<Journal>(&path)? else {
            continue;
        };
        if !journal.retired && native::owner_alive(journal.owner_pid, journal.owner_birth)? {
            continue;
        }
        recover(&path)?;
    }
    Ok(())
}
