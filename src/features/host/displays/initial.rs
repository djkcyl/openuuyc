//! Initial layout policy, before capabilities are probed or media tracks start.
use super::*;
use crate::features::stream_control::publisher::{ConnectOptions, Resolution};

fn size(value: Option<&Resolution>) -> Option<(u32, u32)> {
    let value = value?;
    let (w, h) = (
        u32::try_from(value.width).ok()?,
        u32::try_from(value.height).ok()?,
    );
    (w > 0 && h > 0).then_some((w, h))
}

impl Session {
    pub(crate) async fn prepare(&self, options: ConnectOptions) -> Result<capture::Screen> {
        tokio::task::spawn_blocking(recovery::recover_abandoned).await??;
        ensure!(
            options.kind == 1 && options.connect_type == 1,
            "不支持的被控连接类型"
        );
        ensure!(options.virtual_modes.len() <= 64, "虚拟屏模式数量过多");
        if let Some(params) = &options.params {
            ensure!((0..=4).contains(&params.resolution_type), "无效分辨率策略");
            if params.resolution_type == 4 {
                validate_modes(&[
                    size(params.chosen_resolution.as_ref()).context("缺少指定分辨率")?
                ])?;
            }
        }
        let modes: Vec<_> = options
            .virtual_modes
            .iter()
            .map(|m| Ok((u32::try_from(m.width)?, u32::try_from(m.height)?)))
            .collect::<Result<_>>()?;
        if !modes.is_empty() {
            validate_modes(&modes)?;
        }
        self.run(move |state, lease| state.prepare(options, modes, lease))
            .await
    }
    pub(crate) fn initial_screen(&self, screen: &capture::Screen) -> capture::Screen {
        let state = lock(&self.state);
        let mut initial = screen.clone();
        if let Some(saved) = state
            .journal
            .baseline
            .targets
            .iter()
            .find(|t| screen.identity.as_deref() == Some(t.identity.as_str()))
        {
            initial.width = saved.width;
            initial.height = saved.height;
            initial.left = saved.left;
            initial.top = saved.top;
            initial.dpi_scale = state
                .journal
                .dpi
                .iter()
                .find(|(id, _)| id == &saved.identity)
                .map(|(_, dpi)| *dpi);
        }
        initial
    }
}

impl State {
    fn prepare(
        &mut self,
        options: ConnectOptions,
        modes: Vec<(u32, u32)>,
        lease: &Lease,
    ) -> Result<capture::Screen> {
        let screens = capture::screens()?;
        let choice = options.params.as_ref().map_or(0, |p| p.resolution_type);
        let local = size(
            options
                .params
                .as_ref()
                .and_then(|p| p.local_resolution.as_ref()),
        );
        let virtual_size = if choice == 0 {
            size(options.virtual_initial.as_ref())
        } else {
            local
        }
        .unwrap_or((1920, 1080));
        if !options.force_virtual && self.preferences.super_enabled {
            return self.enter_super(virtual_size.0, virtual_size.1, 0, true, lease);
        }
        if options.force_virtual || self.preferences.default_virtual || screens.is_empty() {
            validate_modes(&[virtual_size])?;
            let result = self.create(
                Virtual {
                    guid: uuid::Uuid::new_v4().to_string(),
                    identity: None,
                    width: virtual_size.0,
                    height: virtual_size.1,
                    hz: 144,
                    dpi: 0,
                    kind: 3,
                    resolution_type: choice.max(1),
                    modes,
                    layout: None,
                },
                lease,
                options.force_virtual || self.preferences.default_virtual,
            );
            if result.is_ok() && options.force_virtual && !self.has_saved_preferences {
                self.preferences.default_virtual = true;
                self.save_preferences()?;
            }
            return result;
        }
        let saved = self.preferences.manual.clone();
        ensure!(
            saved.len() <= 3 && saved.len() + screens.len() <= 5,
            "已保存布局超出屏幕数量上限"
        );
        self.restoring = true;
        let restore = (|| -> Result<()> {
            for mut spec in saved {
                validate_modes(&[(spec.width, spec.height)])?;
                spec.identity = None;
                self.create(spec, lease, false)?;
            }
            // Only restore explicit settings for targets which still exist.
            for saved in self.preferences.physical.clone() {
                if let Some(target) = Topology::query(true)?
                    .targets()?
                    .into_iter()
                    .find(|t| t.identity == saved.identity)
                {
                    self.begin()?;
                    if target
                        .modes
                        .iter()
                        .any(|m| m.width == saved.width && m.height == saved.height)
                    {
                        self.intend_mode(&target.identity, saved.width, saved.height)?;
                        target.set_resolution(saved.width, saved.height, || lease.requested())?;
                    }
                    if saved.dpi > 0 {
                        self.intend_dpi(&target.identity, saved.dpi)?;
                        target.set_dpi(saved.dpi, || lease.requested())?;
                    }
                    self.applied()?;
                }
            }
            Ok(())
        })();
        self.restoring = false;
        restore?;
        let mut screens = capture::screens()?;
        for screen in &mut screens {
            if let Some(owned) =
                self.journal.owned.iter().find(|o| {
                    o.identity.is_some() && o.identity.as_ref() == screen.identity.as_ref()
                })
            {
                screen.id = display::source_id(&format!("openuuyc-virtual/{}", owned.guid))?;
                screen.render_adapter = self.render_adapter;
            }
        }
        let selected = if options.screen_id == -1 {
            screens.iter().find(|s| s.primary).or(screens.first())
        } else {
            screens.iter().find(|s| s.id == options.screen_id)
        }
        .cloned()
        .context("请求的显示器不可用")?;
        // The saved controller layout owns initial physical/virtual modes.
        // Incoming resolution choices apply only when no layout was saved.
        if self.has_saved_preferences {
            return Ok(selected);
        }
        let requested = match choice {
            2 => local,
            4 => size(
                options
                    .params
                    .as_ref()
                    .and_then(|p| p.chosen_resolution.as_ref()),
            ),
            _ => None,
        };
        if let Some((width, height)) = requested {
            validate_modes(&[(width, height)])?;
            let target = Topology::query(true)?
                .targets()?
                .into_iter()
                .find(|t| selected.identity.as_deref() == Some(t.identity.as_str()))
                .context("缺少初始显示目标")?;
            if !target
                .modes
                .iter()
                .any(|m| m.width == width && m.height == height)
            {
                return self.enter_super(width, height, 0, false, lease);
            }
            self.begin()?;
            self.intend_mode(&target.identity, width, height)?;
            target.set_resolution(width, height, || lease.requested())?;
            self.applied()?;
            self.remember_physical(&target.identity, Some(choice))?;
            return find_screen(&target.identity);
        }
        Ok(selected)
    }
}
