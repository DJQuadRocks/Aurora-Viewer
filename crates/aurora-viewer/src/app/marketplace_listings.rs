//! Application bridge for the Marketplace listings window: the SLM requests
//! on the session (never on this thread), their answers, the alerts in the
//! notifications, and the demo's simulated SLM server.

use super::*;
use crate::world::inventory::actions;
use crate::world::marketplace::copy_queue::{self, Transfer};
use crate::world::marketplace::{self as mp, batch, demo};
use crate::world::notifications::{Data, Kind};
use ui::inventory::InvAction;
use ui::marketplace_listings::{Action, CreateStart, Prompt};
use uuid::Uuid;

/// The setting of a listing question (Preferences › Confirmations).
fn ask_setting(s: &mut crate::settings::Settings, prompt: Prompt) -> &mut bool {
    match prompt {
        Prompt::MoveInventory => &mut s.confirm_merchant_move,
        Prompt::ActiveChange => &mut s.confirm_merchant_active_change,
        Prompt::ClearVersion => &mut s.confirm_merchant_clear_version,
        Prompt::ListingCutOrDelete => &mut s.confirm_merchant_listing_delete,
        Prompt::Unlist => &mut s.confirm_merchant_unlist,
    }
}

impl App {
    pub(super) fn prepare_inventory_marketplace_commands(&mut self) {
        self.inventory_ui.marketplace_commands.clear();
        let Some(root) = mp::root(&self.world.inventory) else { return };
        let mut ids = std::collections::HashSet::new();
        if let Some(anchor) = self.inventory_ui.selected {
            ids.extend(self.inventory_ui.selected_ids(anchor));
        }
        if self.panels.inventory_dual
            && let Some(anchor) = self.dual_inventory_ui.selected
        {
            ids.extend(self.dual_inventory_ui.selected_ids(anchor));
        }
        for window in &self.inventory_ui.windows {
            if let Some(anchor) = window.state.selected {
                ids.extend(window.state.selected_ids(anchor));
            }
        }
        if ids.iter().any(|id| mp::depth(&self.world.inventory, root, *id) >= 0) {
            if self.listings_ui.data.initialize() {
                self.send(NetCommand::RequestMerchantStatus);
            }
            if self.listings_ui.data.is_merchant() {
                self.listings_ui.data.fetch_contents(root);
            }
        }
        for id in ids {
            let options = self.listings_ui.inventory_commands(&self.world.inventory, root, id);
            if !options.is_empty() {
                self.inventory_ui.marketplace_commands.insert(id, options);
            }
        }
    }

    pub(super) fn inventory_marketplace_command(&mut self, item: uuid::Uuid, command: ui::marketplace_listings::Command) {
        let Some(root) = mp::root(&self.world.inventory) else { return };
        let worn = self.worn_items();
        let mut actions = Vec::new();
        // shown again, a minimized window expands (LLFloaterReg::showInstance)
        ui::widgets::open_floater(&self.egui_ctx, ui::marketplace_listings::WINDOW, &mut self.listings_ui.open);
        self.listings_ui
            .inventory_command(&self.world.inventory, &worn, root, item, command, &mut actions);
        self.apply_listing_actions(actions);
    }

    fn apply_listing_actions(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Edit(change) => self.apply_inventory_action(InvAction::Edit(change)),
                Action::Delete(deletion) => self.submit_inventory_delete(*deletion),
                Action::Paste { change, cut, editor } => self.submit_marketplace_paste(change, cut, editor),
                Action::OpenMarketplace(url) => self.open_web_link(url, Some(crate::link_trust::BrowserEntryPoint::MarketplaceWindow)),
                Action::OpenExternal(url) => self.on_ctx_action(ui::context::CtxAction::OpenUrlExternal(url)),
                Action::Notify { text, url } => self.marketplace_alert(text, url),
                Action::CreateRoot => self.open_marketplace_listings(),
            }
        }
    }

    /// One copy or move into Marketplace listings, repeated `count` times for
    /// a copy. Each step is `actions::marketplace`. New folders are queued for
    /// POST /listings when the merchant asked for listings.
    pub(super) fn start_marketplace_transfer(&mut self, ids: Vec<Uuid>, copy: bool, count: u32, create_listings: bool) {
        if self.listings_ui.copy_pump.active() {
            self.listings_ui.message = aurora_i18n::tr!("marketplace-copy-busy").into();
            self.listings_ui.open = true;
            return;
        }
        let worn = self.worn_items();
        if let Err(reason) = actions::marketplace(&self.world.inventory, &ids, copy, &self.settings.inventory.protected, &worn) {
            self.listings_ui.message = reason;
            self.listings_ui.open = true;
            return;
        }
        if self
            .listings_ui
            .copy_pump
            .start(Transfer {
                ids,
                copy,
                count,
                create_listings,
            })
            .is_err()
        {
            self.listings_ui.message = aurora_i18n::tr!("marketplace-copy-count-invalid").into();
        }
        self.listings_ui.open = true;
        self.pump_marketplace_copies();
    }

    pub(super) fn marketplace_copy_landed(&mut self, folders: Option<Vec<Uuid>>) {
        let Some(folders) = folders else { return };
        let folders: Vec<_> = folders
            .into_iter()
            .filter(|id| self.world.inventory.folders.contains_key(id))
            .collect();
        if folders.is_empty() {
            return;
        }
        self.listings_ui.creates.push(folders);
        self.listings_ui.open = true;
    }

    /// The next copy, once the inventory is free. A question holds the step.
    fn pump_marketplace_copies(&mut self) {
        let asking = self.listings_ui.holds_question();
        let pending = self.inventory_ui.pending;
        self.listings_ui.copy_pump.poll_guard(pending, asking);
        if self.inventory_ui.mutation_busy() {
            return;
        }
        let Some((ids, copy)) = self.listings_ui.copy_pump.next_ids() else {
            return;
        };
        let worn = self.worn_items();
        let root = mp::root(&self.world.inventory);
        match actions::marketplace(&self.world.inventory, &ids, copy, &self.settings.inventory.protected, &worn) {
            Err(reason) => {
                self.listings_ui.copy_pump.reject();
                self.listings_ui.message = reason;
            }
            Ok(change) => {
                let folders = copy_queue::staged_listing_folders(&change, root);
                let copies = !change.copies.is_empty();
                self.edit_inventory(change, false, Uuid::nil());
                if let Some(request) = self.inventory_ui.pending {
                    self.listings_ui.copy_pump.submitted(request, folders, copies);
                } else if self.listings_ui.holds_question() {
                    self.listings_ui.copy_pump.note_held(folders, copies);
                } else {
                    let reason = self.inventory_ui.message.clone();
                    self.listings_ui.copy_pump.reject();
                    self.listings_ui.message = if reason.is_empty() {
                        aurora_i18n::tr!("marketplace-copy-stopped").into()
                    } else {
                        reason
                    };
                }
            }
        }
    }

    /// One POST /listings when the pause is over and the inventory can take
    /// the audit's fix. The first of a batch leaves at once.
    fn pump_listing_creates(&mut self) {
        if self.listings_ui.creates.inflight().is_none() && self.inventory_ui.mutation_busy() {
            return;
        }
        let now = std::time::Instant::now();
        let Some(folder) = self.listings_ui.creates.drive(now) else {
            return;
        };
        let Some(root) = mp::root(&self.world.inventory) else {
            let gap = self.listings_ui.next_gap();
            self.listings_ui.creates.failed(now, "no listings folder".into(), gap);
            return;
        };
        let worn = self.worn_items();
        let mut actions = Vec::new();
        let start = self
            .listings_ui
            .begin_create(&self.world.inventory, &worn, root, folder, &mut actions);
        self.apply_listing_actions(actions);
        let gap = self.listings_ui.next_gap();
        match start {
            CreateStart::Sent => {}
            CreateStart::AlreadyListed => self.listings_ui.creates.succeeded(now, gap),
            CreateStart::Failed(error) => {
                let name = self.listing_folder_name(folder);
                self.listings_ui.creates.failed(now, error.clone(), gap);
                self.marketplace_alert(aurora_i18n::tr!("marketplace-create-failed", name = name, error = error), None);
            }
        }
    }

    fn listing_folder_name(&self, folder: uuid::Uuid) -> String {
        self.world
            .inventory
            .folders
            .get(&folder)
            .map(|folder| folder.info.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| aurora_i18n::tr!("marketplace-listings-new-folder-name").into())
    }

    fn finish_batch_slm(&mut self, request: &aurora_net::slm::SlmRequest, reply: aurora_net::slm::SlmReply) {
        use aurora_net::slm::{SlmReply, SlmRequest};
        let SlmRequest::Create { folder, .. } = request else {
            return;
        };
        let folder = *folder;
        let now = std::time::Instant::now();
        let gap = self.listings_ui.next_gap();
        if matches!(reply, SlmReply::RateLimited) {
            self.listings_ui.data.note_rate_limit(request);
            if !self.listings_ui.creates.rate_limited(now, batch::RATE_LIMIT_BACKOFF, gap) {
                let name = self.listing_folder_name(folder);
                self.marketplace_alert(aurora_i18n::tr!("marketplace-create-rate-limit-gave-up", name = name), None);
            }
            return;
        }
        let root = mp::root(&self.world.inventory).unwrap_or_default();
        self.listings_ui
            .data
            .apply(&self.world.inventory, root, request, reply.clone(), now);
        match reply {
            SlmReply::Failed(_) | SlmReply::NotFound => {
                let alerts = self.listings_ui.data.take_alerts();
                let error = alerts
                    .first()
                    .map(|alert| ui::marketplace_listings::alert_text(alert).0)
                    .unwrap_or_else(|| aurora_i18n::tr!("marketplace-alert-rate-limited").into());
                let name = self.listing_folder_name(folder);
                self.listings_ui.creates.failed(now, error.clone(), gap);
                self.marketplace_alert(aurora_i18n::tr!("marketplace-create-failed", name = name, error = error), None);
            }
            SlmReply::Listings(_) => {
                for alert in self.listings_ui.data.take_alerts() {
                    let (text, url) = ui::marketplace_listings::alert_text(&alert);
                    self.marketplace_alert(text, url);
                }
                self.listings_ui.creates.succeeded(now, gap);
            }
            // Handled above: a batch retry stays quiet and does not toast.
            SlmReply::RateLimited => {}
        }
    }
    /// Me › Marketplace listings (LLFloaterMarketplaceListings::onOpen).
    pub(super) fn open_marketplace_listings(&mut self) {
        if self.listings_ui.open(&self.egui_ctx) {
            self.send(NetCommand::RequestMerchantStatus);
        }
    }

    /// Every frame: the listings follow the inventory, the queued SLM
    /// requests go to the session, the alerts to the notifications, the
    /// inventory learns which listing folders stay in place, and the
    /// listings which questions are still asked.
    pub(super) fn update_marketplace_listings(&mut self) {
        self.listings_ui.reconcile(&self.world.inventory);
        let holds = self.listings_ui.data.holds();
        if self.world.inventory.listing_holds != holds {
            self.world.inventory.listing_holds = holds;
        }
        if let Some(prompt) = self.listings_ui.silence.take() {
            *ask_setting(&mut self.settings, prompt) = false;
            self.settings.save();
        }
        self.listings_ui.silenced.clear();
        for prompt in Prompt::ALL {
            if !*ask_setting(&mut self.settings, prompt) {
                self.listings_ui.silenced.insert(prompt);
            }
        }
        self.pump_marketplace_copies();
        self.pump_listing_creates();
        for request in self.listings_ui.data.take_requests(std::time::Instant::now()) {
            log::info!("SLM request queued: {}", request.name());
            self.send(NetCommand::Slm(request));
        }
        for alert in self.listings_ui.data.take_alerts() {
            let (text, url) = ui::marketplace_listings::alert_text(&alert);
            self.marketplace_alert(text, url);
        }
        if self.demo {
            self.demo_marketplace_seed();
        }
        // Use the application's frame clock: egui's count starts later
        // than the demo and its capture/loading clock.
        if self.demo && self.frame_count >= 680 {
            let mode = std::env::var("AURORA_DEMO_MARKETPLACE_LISTINGS").unwrap_or_default();
            let menu = match mode.as_str() {
                "menu-listing" => Some(demo::HOODIE),
                "menu-version" => Some(demo::HOODIE_V2),
                "menu-item" => Some(uuid::Uuid::from_u128(8_400_102)),
                _ => None,
            };
            if let Some(id) = menu {
                self.listings_ui.demo_menu(id);
            }
        }
        if self.demo && std::env::var("AURORA_DEMO_MARKETPLACE_LISTINGS").is_ok_and(|v| v == "audit") {
            let worn = self.worn_items();
            self.listings_ui.demo_audit(&self.world.inventory, &worn);
        }
    }

    /// NetEvent::MerchantStatus and NetEvent::Slm; false for other events.
    pub(super) fn marketplace_listings_event(&mut self, ev: &NetEvent) -> bool {
        match ev {
            NetEvent::MerchantStatus(status) => {
                log::info!("merchant status: {status:?}");
                self.inventory_ui.merchant = *status == aurora_net::slm::MerchantStatus::Merchant;
                self.listings_ui.data.set_status(status.clone());
                true
            }
            NetEvent::Slm { request, reply } => {
                let owned = matches!(request, aurora_net::slm::SlmRequest::Create { folder, .. } if self.listings_ui.creates.owns(*folder));
                if owned {
                    self.finish_batch_slm(request, reply.clone());
                } else {
                    let root = mp::root(&self.world.inventory).unwrap_or_default();
                    self.listings_ui
                        .data
                        .apply(&self.world.inventory, root, request, reply.clone(), std::time::Instant::now());
                }
                true
            }
            _ => false,
        }
    }

    fn worn_items(&self) -> std::collections::HashSet<uuid::Uuid> {
        ui::appearance::items::worn_labels(&self.world).into_keys().collect()
    }

    fn marketplace_alert(&mut self, text: String, url: Option<String>) {
        self.world.notifications.push(
            Kind::Alert,
            aurora_i18n::tr!("marketplace-listings-title"),
            text,
            url.map_or(Data::None, Data::Url),
        );
    }

    pub(super) fn show_marketplace_listings(&mut self, ctx: &egui::Context, p: &crate::theme::Palette) {
        if !self.listings_ui.open {
            return;
        }
        let worn = self.worn_items();
        let home = self.marketplace_home();
        let preferences = self.settings.inventory.clone();
        let mut window = self.inventory_ui.take_listings_panel();
        let facts = ui::inventory::Facts::from_world(&self.world, self.appearance_ui.pending.is_some());
        let mut panel = ui::inventory::listings::Panel {
            state: &mut window.state,
            prefs: &mut self.settings.inventory,
            facts: &facts,
            actions: Vec::new(),
        };
        let actions = self.listings_ui.show(
            ctx,
            p,
            &self.skin.icons,
            &mut self.world.inventory,
            &worn,
            home.as_deref(),
            &mut panel,
        );
        let inventory_actions = std::mem::take(&mut panel.actions);
        self.inventory_ui.put_listings_panel(window);
        self.inventory_ui.preferences_dirty |= self.settings.inventory != preferences;
        for action in inventory_actions {
            self.apply_inventory_action(action);
        }
        for action in actions {
            match action {
                Action::CreateRoot => {
                    let id = uuid::Uuid::new_v4();
                    let root = self.world.inventory.root;
                    self.apply_inventory_action(InvAction::Edit(aurora_net::inventory::operations::Mutation {
                        operations: vec![aurora_net::inventory::operations::Operation::Folder {
                            id,
                            parent: root,
                            name: aurora_i18n::tr!("common-marketplace-listings").into(),
                            type_default: mp::LISTINGS,
                        }],
                        refresh: vec![root, id],
                        ..Default::default()
                    }));
                }
                Action::Edit(change) => self.submit_inventory_change(change),
                Action::Delete(deletion) => self.submit_inventory_delete(*deletion),
                Action::Paste { change, cut, editor } => self.submit_marketplace_paste(change, cut, editor),
                Action::OpenMarketplace(url) => self.open_web_link(url, Some(crate::link_trust::BrowserEntryPoint::MarketplaceWindow)),
                Action::OpenExternal(url) => self.on_ctx_action(ui::context::CtxAction::OpenUrlExternal(url)),
                Action::Notify { text, url } => self.marketplace_alert(text, url),
            }
        }
        if !self.inventory_ui.message.is_empty() && self.listings_ui.message.is_empty() {
            self.listings_ui.message.clone_from(&self.inventory_ui.message);
        }
    }

    pub(super) fn paste_marketplace_inventory(&mut self, to: uuid::Uuid, ids: Vec<uuid::Uuid>, copy: bool) {
        let worn = self.worn_items();
        let mut actions = Vec::new();
        self.listings_ui.paste_inventory(
            &self.world.inventory,
            &worn,
            &self.settings.inventory.protected,
            to,
            &ids,
            copy,
            false,
            &mut actions,
        );
        for action in actions {
            match action {
                Action::Paste { change, cut, editor } => self.submit_marketplace_paste(change, cut, editor),
                Action::Notify { text, url } => self.marketplace_alert(text, url),
                _ => {}
            }
        }
    }

    /// AURORA_DEMO_MARKETPLACE_LISTINGS (docs/TESTING.md): the sample
    /// listings and the window open at start.
    pub(super) fn demo_marketplace_listings(&mut self) {
        let Some(mode) = demo::Mode::from_env() else {
            return;
        };
        self.demo_slm = Some(demo::DemoSlm::new(mode));
        // a merchant's window opens once the sample listings are in the
        // inventory (`demo_marketplace_seed`)
        if mode != demo::Mode::Merchant {
            self.open_marketplace_listings();
        }
    }

    /// The sample listings, once the demo inventory has its root.
    fn demo_marketplace_seed(&mut self) {
        let Some(slm) = self.demo_slm.as_mut() else { return };
        if slm.mode != demo::Mode::Merchant || slm.seeded || self.world.inventory.root.is_nil() {
            return;
        }
        slm.seeded = true;
        demo::seed(&mut self.world.inventory, self.world.agent_id);
        self.listings_ui
            .demo_expand(&[demo::HOODIE, demo::HOODIE_V2, demo::POSTER, demo::POSTER_VERSION, demo::LANTERN]);
        self.open_marketplace_listings();
    }

    /// The demo's SLM server: GET /merchant and the listings requests are
    /// answered in the viewer, from the sample listings.
    pub(super) fn demo_marketplace_command(&mut self, cmd: &NetCommand) -> bool {
        let slm = self
            .demo_slm
            .get_or_insert_with(|| demo::DemoSlm::new(demo::Mode::from_env().unwrap_or(demo::Mode::NotMerchant)));
        match cmd {
            NetCommand::RequestMerchantStatus => {
                let status = slm.merchant();
                self.on_app_event(NetEvent::MerchantStatus(status));
                true
            }
            NetCommand::Slm(request) => {
                log::info!("demo action: SLM {} (simulated)", request.name());
                let reply = slm.answer(request);
                self.on_app_event(NetEvent::Slm {
                    request: request.clone(),
                    reply,
                });
                true
            }
            _ => false,
        }
    }
}
