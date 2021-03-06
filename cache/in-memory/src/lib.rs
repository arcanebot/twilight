//! # twilight-cache-inmemory
//!
//! [![discord badge][]][discord link] [![github badge][]][github link] [![license badge][]][license link] ![rust badge]
//!
//! `twilight-cache-inmemory` is an in-process-memory cache for the
//! [`twilight-rs`] ecosystem. It's responsible for processing events and
//! caching things like guilds, channels, users, and voice states.
//!
//! ## Examples
//!
//! Update a cache with events that come in through the gateway:
//!
//! ```rust,no_run
//! use std::env;
//! use futures::stream::StreamExt;
//! use twilight_cache_inmemory::InMemoryCache;
//! use twilight_gateway::{Intents, Shard};
//!
//! # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let token = env::var("DISCORD_TOKEN")?;
//! let (shard, mut events) = Shard::new(token, Intents::GUILD_MESSAGES);
//! shard.start().await?;
//!
//! // Create a cache, caching up to 10 messages per channel:
//! let cache = InMemoryCache::builder().message_cache_size(10).build();
//!
//! while let Some(event) = events.next().await {
//!     // Update the cache with the event.
//!     cache.update(&event);
//! }
//! # Ok(()) }
//! ```
//!
//! ## License
//!
//! All first-party crates are licensed under [ISC][LICENSE.md]
//!
//! [LICENSE.md]: https://github.com/twilight-rs/twilight/blob/main/LICENSE.md
//! [discord badge]: https://img.shields.io/discord/745809834183753828?color=%237289DA&label=discord%20server&logo=discord&style=for-the-badge
//! [discord link]: https://discord.gg/7jj8n7D
//! [docs:discord:sharding]: https://discord.com/developers/docs/topics/gateway#sharding
//! [github badge]: https://img.shields.io/badge/github-twilight-6f42c1.svg?style=for-the-badge&logo=github
//! [github link]: https://github.com/twilight-rs/twilight
//! [license badge]: https://img.shields.io/badge/license-ISC-blue.svg?style=for-the-badge&logo=pastebin
//! [license link]: https://github.com/twilight-rs/twilight/blob/main/LICENSE.md
//! [rust badge]: https://img.shields.io/badge/rust-1.49+-93450a.svg?style=for-the-badge&logo=rust

#![deny(
    clippy::missing_const_for_fn,
    broken_intra_doc_links,
    rust_2018_idioms,
    unused,
    warnings
)]

pub mod model;

mod builder;
mod config;
mod stats;
mod updates;

pub use self::{
    builder::InMemoryCacheBuilder,
    config::{Config, ResourceType},
    stats::InMemoryCacheStats,
    updates::UpdateCache,
};

use self::model::*;
use dashmap::{
    mapref::{entry::Entry, one::Ref},
    DashMap, DashSet,
};
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashSet, VecDeque},
    hash::Hash,
    sync::{Arc, Mutex},
};
use twilight_model::{
    application::interaction::application_command::InteractionMember,
    channel::{Group, GuildChannel, PrivateChannel, StageInstance},
    gateway::presence::UserOrId,
    guild::{Emoji, Guild, GuildIntegration, Member, PartialMember, Role},
    id::{ChannelId, EmojiId, GuildId, IntegrationId, MessageId, RoleId, StageId, UserId},
    user::{CurrentUser, User},
    voice::VoiceState,
};

#[derive(Debug)]
struct GuildItem<T> {
    data: T,
    guild_id: GuildId,
}

fn upsert_guild_item<K: Eq + Hash, V: PartialEq>(
    map: &DashMap<K, GuildItem<V>>,
    guild_id: GuildId,
    key: K,
    value: V,
) {
    match map.entry(key) {
        Entry::Occupied(entry) if entry.get().data == value => {}
        Entry::Occupied(mut entry) => {
            entry.insert(GuildItem {
                data: value,
                guild_id,
            });
        }
        Entry::Vacant(entry) => {
            entry.insert(GuildItem {
                data: value,
                guild_id,
            });
        }
    }
}

fn upsert_item<K: Eq + Hash, V: PartialEq>(map: &DashMap<K, V>, k: K, v: V) {
    map.insert(k, v);
}

// When adding a field here, be sure to add it to `InMemoryCache::clear` if
// necessary.
#[derive(Debug, Default)]
struct InMemoryCacheRef {
    config: Config,
    channels_guild: DashMap<ChannelId, GuildItem<GuildChannel>>,
    channels_private: DashMap<ChannelId, PrivateChannel>,
    // So long as the lock isn't held across await or panic points this is fine.
    current_user: Mutex<Option<CurrentUser>>,
    emojis: DashMap<EmojiId, GuildItem<CachedEmoji>>,
    groups: DashMap<ChannelId, Group>,
    guilds: DashMap<GuildId, CachedGuild>,
    guild_channels: DashMap<GuildId, HashSet<ChannelId>>,
    guild_emojis: DashMap<GuildId, HashSet<EmojiId>>,
    guild_integrations: DashMap<GuildId, HashSet<IntegrationId>>,
    guild_members: DashMap<GuildId, HashSet<UserId>>,
    guild_presences: DashMap<GuildId, HashSet<UserId>>,
    guild_roles: DashMap<GuildId, HashSet<RoleId>>,
    guild_stage_instances: DashMap<GuildId, HashSet<StageId>>,
    integrations: DashMap<(GuildId, IntegrationId), GuildItem<GuildIntegration>>,
    members: DashMap<(GuildId, UserId), CachedMember>,
    messages: DashMap<ChannelId, VecDeque<CachedMessage>>,
    presences: DashMap<(GuildId, UserId), CachedPresence>,
    roles: DashMap<RoleId, GuildItem<Role>>,
    stage_instances: DashMap<StageId, GuildItem<StageInstance>>,
    unavailable_guilds: DashSet<GuildId>,
    users: DashMap<UserId, (User, BTreeSet<GuildId>)>,
    /// Mapping of channels and the users currently connected.
    voice_state_channels: DashMap<ChannelId, HashSet<(GuildId, UserId)>>,
    /// Mapping of guilds and users currently connected to its voice channels.
    voice_state_guilds: DashMap<GuildId, HashSet<UserId>>,
    /// Mapping of guild ID and user ID pairs to their voice states.
    voice_states: DashMap<(GuildId, UserId), VoiceState>,
}

/// A thread-safe, in-memory-process cache of Discord data. It can be cloned and
/// sent to other threads.
///
/// This is an implementation of a cache designed to be used by only the
/// current process.
///
/// Events will only be processed if they are properly expressed with
/// [`Intents`]; refer to function-level documentation for more details.
///
/// # Cloning
///
/// The cache internally wraps its data within an Arc. This means that the cache
/// can be cloned and passed around tasks and threads cheaply.
///
/// # Design and Performance
///
/// The defining characteristic of this cache is that returned types (such as a
/// guild or user) do not use locking for access. The internals of the cache use
/// a concurrent map for mutability and the returned types are clones of the
/// cached data. If a user is retrieved from the cache, then a clone of the user
/// *at that point in time* is returned. If the cache updates the user, then the
/// returned user  held by you will be outdated.
///
/// The intended use is that data is held outside the cache for only as long
/// as necessary, where the state of the value at that point time doesn't need
/// to be up-to-date. If you need to ensure you always have the most up-to-date
/// "version" of a cached resource, then you can re-retrieve it whenever you use
/// it: retrieval operations are extremely cheap.
///
/// For example, say you're deleting some of the guilds of a channel. You'll
/// probably need the guild to do that, so you retrieve it from the cache. You
/// can then use the guild to update all of the channels, because for most use
/// cases you don't need the guild to be up-to-date in real time, you only need
/// its state at that *point in time* or maybe across the lifetime of an
/// operation. If you need the guild to always be up-to-date between operations,
/// then the intent is that you keep getting it from the cache.
///
/// [`Intents`]: ::twilight_model::gateway::Intents
#[derive(Clone, Debug, Default)]
pub struct InMemoryCache(Arc<InMemoryCacheRef>);

/// Implemented methods and types for the cache.
impl InMemoryCache {
    /// Creates a new, empty cache.
    ///
    /// # Examples
    ///
    /// Creating a new `InMemoryCache` with a custom configuration, limiting
    /// the message cache to 50 messages per channel:
    ///
    /// ```
    /// use twilight_cache_inmemory::InMemoryCache;
    ///
    /// let cache = InMemoryCache::builder().message_cache_size(50).build();
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    fn new_with_config(config: Config) -> Self {
        Self(Arc::new(InMemoryCacheRef {
            config,
            ..Default::default()
        }))
    }

    /// Create a new builder to configure and construct an in-memory cache.
    pub const fn builder() -> InMemoryCacheBuilder {
        InMemoryCacheBuilder::new()
    }

    /// Returns a copy of the config cache.
    pub fn config(&self) -> Config {
        self.0.config.clone()
    }

    /// Create an interface for retrieving statistics about the cache.
    ///
    /// # Examples
    ///
    /// Print the number of guilds in a cache:
    ///
    /// ```
    /// use twilight_cache_inmemory::InMemoryCache;
    ///
    /// let cache = InMemoryCache::new();
    ///
    /// // later on...
    /// let guilds = cache.stats().guilds();
    /// println!("guild count: {}", guilds);
    /// ```
    pub const fn stats(&self) -> InMemoryCacheStats<'_> {
        InMemoryCacheStats::new(self)
    }

    /// Update the cache with an event from the gateway.
    pub fn update(&self, value: &impl UpdateCache) {
        value.update(self);
    }

    /// Gets a channel by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    pub fn guild_channel(&self, channel_id: ChannelId) -> Option<GuildChannel> {
        self.0
            .channels_guild
            .get(&channel_id)
            .map(|r| r.data.clone())
    }

    /// Gets the current user.
    ///
    /// This is an O(1) operation.
    pub fn current_user(&self) -> Option<CurrentUser> {
        self.0
            .current_user
            .lock()
            .expect("current user poisoned")
            .clone()
    }

    /// Gets an emoji by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILD_EMOJIS`] intent.
    ///
    /// [`GUILD_EMOJIS`]: ::twilight_model::gateway::Intents::GUILD_EMOJIS
    pub fn emoji(&self, emoji_id: EmojiId) -> Option<CachedEmoji> {
        self.0.emojis.get(&emoji_id).map(|r| r.data.clone())
    }

    /// Gets a group by ID.
    ///
    /// This is an O(1) operation.
    pub fn group(&self, channel_id: ChannelId) -> Option<Group> {
        self.0.groups.get(&channel_id).map(|r| r.clone())
    }

    /// Gets a guild by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    pub fn guild(&self, guild_id: GuildId) -> Option<CachedGuild> {
        self.0.guilds.get(&guild_id).map(|r| r.clone())
    }

    /// Gets the set of channels in a guild.
    ///
    /// This is a O(m) operation, where m is the amount of channels in the
    /// guild. This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    pub fn guild_channels(&self, guild_id: GuildId) -> Option<HashSet<ChannelId>> {
        self.0.guild_channels.get(&guild_id).map(|r| r.clone())
    }

    /// Gets the set of emojis in a guild.
    ///
    /// This is a O(m) operation, where m is the amount of emojis in the guild.
    /// This requires both the [`GUILDS`] and [`GUILD_EMOJIS`] intents.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    /// [`GUILD_EMOJIS`]: ::twilight_model::gateway::Intents::GUILD_EMOJIS
    pub fn guild_emojis(&self, guild_id: GuildId) -> Option<HashSet<EmojiId>> {
        self.0.guild_emojis.get(&guild_id).map(|r| r.clone())
    }

    /// Gets the set of members in a guild.
    ///
    /// This list may be incomplete if not all members have been cached.
    ///
    /// This is a O(m) operation, where m is the amount of members in the guild.
    /// This requires the [`GUILD_MEMBERS`] intent.
    ///
    /// [`GUILD_MEMBERS`]: ::twilight_model::gateway::Intents::GUILD_MEMBERS
    pub fn guild_members(&self, guild_id: GuildId) -> Option<HashSet<UserId>> {
        self.0.guild_members.get(&guild_id).map(|r| r.clone())
    }

    /// Gets the set of presences in a guild.
    ///
    /// This list may be incomplete if not all members have been cached.
    ///
    /// This is a O(m) operation, where m is the amount of members in the guild.
    /// This requires the [`GUILD_PRESENCES`] intent.
    ///
    /// [`GUILD_PRESENCES`]: ::twilight_model::gateway::Intents::GUILD_PRESENCES
    pub fn guild_presences(&self, guild_id: GuildId) -> Option<HashSet<UserId>> {
        self.0.guild_presences.get(&guild_id).map(|r| r.clone())
    }

    /// Gets the set of roles in a guild.
    ///
    /// This is a O(m) operation, where m is the amount of roles in the guild.
    /// This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    pub fn guild_roles(&self, guild_id: GuildId) -> Option<HashSet<RoleId>> {
        self.0.guild_roles.get(&guild_id).map(|r| r.clone())
    }

    /// Gets the set of stage instances in a guild.
    ///
    /// This is a O(m) operation, where m is the amount of stage instances in
    /// the guild. This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: twilight_model::gateway::Intents::GUILDS
    pub fn guild_stage_instances(&self, guild_id: GuildId) -> Option<HashSet<StageId>> {
        self.0
            .guild_stage_instances
            .get(&guild_id)
            .map(|r| r.value().clone())
    }

    /// Gets a member by guild ID and user ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILD_MEMBERS`] intent.
    ///
    /// [`GUILD_MEMBERS`]: ::twilight_model::gateway::Intents::GUILD_MEMBERS
    pub fn member(&self, guild_id: GuildId, user_id: UserId) -> Option<CachedMember> {
        self.0.members.get(&(guild_id, user_id)).map(|r| r.clone())
    }

    /// Gets a message by channel ID and message ID.
    ///
    /// This is an O(n) operation. This requires one or both of the
    /// [`GUILD_MESSAGES`] or [`DIRECT_MESSAGES`] intents.
    ///
    /// [`GUILD_MESSAGES`]: ::twilight_model::gateway::Intents::GUILD_MESSAGES
    /// [`DIRECT_MESSAGES`]: ::twilight_model::gateway::Intents::DIRECT_MESSAGES
    pub fn message(&self, channel_id: ChannelId, message_id: MessageId) -> Option<CachedMessage> {
        let channel = self.0.messages.get(&channel_id)?;

        channel.iter().find(|msg| msg.id == message_id).cloned()
    }

    /// Gets a presence by, optionally, guild ID, and user ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILD_PRESENCES`] intent.
    ///
    /// [`GUILD_PRESENCES`]: ::twilight_model::gateway::Intents::GUILD_PRESENCES
    pub fn presence(&self, guild_id: GuildId, user_id: UserId) -> Option<CachedPresence> {
        self.0
            .presences
            .get(&(guild_id, user_id))
            .map(|r| r.clone())
    }

    /// Gets a private channel by ID.
    ///
    /// This is an O(1) operation. This requires the [`DIRECT_MESSAGES`] intent.
    ///
    /// [`DIRECT_MESSAGES`]: ::twilight_model::gateway::Intents::DIRECT_MESSAGES
    pub fn private_channel(&self, channel_id: ChannelId) -> Option<PrivateChannel> {
        self.0.channels_private.get(&channel_id).map(|r| r.clone())
    }

    /// Gets a role by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    pub fn role(&self, role_id: RoleId) -> Option<Role> {
        self.0.roles.get(&role_id).map(|r| r.data.clone())
    }

    /// Gets a stage instance by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILDS`] intent.
    ///
    /// [`GUILDS`]: twilight_model::gateway::Intents::GUILDS
    pub fn stage_instance(&self, stage_id: StageId) -> Option<StageInstance> {
        self.0
            .stage_instances
            .get(&stage_id)
            .map(|role| role.data.clone())
    }

    /// Gets a user by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILD_MEMBERS`] intent.
    ///
    /// [`GUILD_MEMBERS`]: ::twilight_model::gateway::Intents::GUILD_MEMBERS
    pub fn user(&self, user_id: UserId) -> Option<User> {
        self.0.users.get(&user_id).map(|r| r.0.clone())
    }

    /// Gets a user by ID.
    ///
    /// This is an O(1) operation. This requires the [`GUILD_MEMBERS`] intent.
    ///
    /// [`GUILD_MEMBERS`]: ::twilight_model::gateway::Intents::GUILD_MEMBERS
    pub fn user_ref(&self, user_id: UserId) -> Option<Ref<'_, UserId, (User, BTreeSet<GuildId>)>> {
        self.0.users.get(&user_id)
    }

    /// Gets the voice states within a voice channel.
    ///
    /// This requires both the [`GUILDS`] and [`GUILD_VOICE_STATES`] intents.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    /// [`GUILD_VOICE_STATES`]: ::twilight_model::gateway::Intents::GUILD_VOICE_STATES
    pub fn voice_channel_states(&self, channel_id: ChannelId) -> Option<Vec<VoiceState>> {
        let user_ids = self.0.voice_state_channels.get(&channel_id)?;

        Some(
            user_ids
                .iter()
                .filter_map(|key| self.0.voice_states.get(&key).map(|r| r.clone()))
                .collect(),
        )
    }

    /// Gets a voice state by user ID and Guild ID.
    ///
    /// This is an O(1) operation. This requires both the [`GUILDS`] and
    /// [`GUILD_VOICE_STATES`] intents.
    ///
    /// [`GUILDS`]: ::twilight_model::gateway::Intents::GUILDS
    /// [`GUILD_VOICE_STATES`]: ::twilight_model::gateway::Intents::GUILD_VOICE_STATES
    pub fn voice_state(&self, user_id: UserId, guild_id: GuildId) -> Option<VoiceState> {
        self.0
            .voice_states
            .get(&(guild_id, user_id))
            .map(|r| r.clone())
    }

    /// Clear the state of the Cache.
    ///
    /// This is equal to creating a new empty cache.
    pub fn clear(&self) {
        self.0.channels_guild.clear();
        self.0.channels_private.clear();
        self.0
            .current_user
            .lock()
            .expect("current user poisoned")
            .take();
        self.0.emojis.clear();
        self.0.groups.clear();
        self.0.guilds.clear();
        self.0.guild_channels.clear();
        self.0.guild_emojis.clear();
        self.0.guild_integrations.clear();
        self.0.guild_members.clear();
        self.0.guild_presences.clear();
        self.0.guild_roles.clear();
        self.0.guild_stage_instances.clear();
        self.0.integrations.clear();
        self.0.members.clear();
        self.0.messages.clear();
        self.0.presences.clear();
        self.0.roles.clear();
        self.0.unavailable_guilds.clear();
        self.0.users.clear();
        self.0.voice_state_channels.clear();
        self.0.voice_state_guilds.clear();
        self.0.voice_states.clear();
    }

    fn cache_current_user(&self, current_user: CurrentUser) {
        self.0
            .current_user
            .lock()
            .expect("current user poisoned")
            .replace(current_user);
    }

    fn cache_guild_channels(
        &self,
        guild_id: GuildId,
        guild_channels: impl IntoIterator<Item = GuildChannel>,
    ) {
        for channel in guild_channels {
            self.cache_guild_channel(guild_id, channel);
        }
    }

    fn cache_guild_channel(&self, guild_id: GuildId, mut channel: GuildChannel) {
        match channel {
            GuildChannel::Category(ref mut c) => {
                c.guild_id.replace(guild_id);
            }
            GuildChannel::Text(ref mut c) => {
                c.guild_id.replace(guild_id);
            }
            GuildChannel::Voice(ref mut c) => {
                c.guild_id.replace(guild_id);
            }
            GuildChannel::Stage(ref mut c) => {
                c.guild_id.replace(guild_id);
            }
        }

        let id = channel.id();
        self.0
            .guild_channels
            .entry(guild_id)
            .or_default()
            .insert(id);

        upsert_guild_item(&self.0.channels_guild, guild_id, id, channel);
    }

    fn cache_emoji(&self, guild_id: GuildId, emoji: Emoji) {
        match self.0.emojis.get(&emoji.id) {
            Some(cached_emoji) if cached_emoji.data == emoji => return,
            Some(_) | None => {}
        }

        let user_id = emoji.user.as_ref().map(|user| user.id);

        if let Some(user) = emoji.user {
            self.cache_user(Cow::Owned(user), Some(guild_id));
        }

        let cached = CachedEmoji {
            id: emoji.id,
            animated: emoji.animated,
            name: emoji.name,
            managed: emoji.managed,
            require_colons: emoji.require_colons,
            roles: emoji.roles,
            user_id,
            available: emoji.available,
        };

        self.0.emojis.insert(
            cached.id,
            GuildItem {
                data: cached,
                guild_id,
            },
        );

        self.0
            .guild_emojis
            .entry(guild_id)
            .or_default()
            .insert(emoji.id);
    }

    fn cache_emojis(&self, guild_id: GuildId, emojis: Vec<Emoji>) {
        if let Some(mut guild_emojis) = self.0.guild_emojis.get_mut(&guild_id) {
            let incoming: Vec<EmojiId> = emojis.iter().map(|e| e.id).collect();

            let removal_filter: Vec<EmojiId> = guild_emojis
                .iter()
                .copied()
                .filter(|e| !incoming.contains(e))
                .collect();

            for to_remove in &removal_filter {
                guild_emojis.remove(to_remove);
            }

            for to_remove in &removal_filter {
                self.0.emojis.remove(to_remove);
            }
        }

        for emoji in emojis {
            self.cache_emoji(guild_id, emoji);
        }
    }

    fn cache_group(&self, group: Group) {
        upsert_item(&self.0.groups, group.id, group)
    }

    fn cache_guild(&self, guild: Guild) {
        // The map and set creation needs to occur first, so caching states and
        // objects always has a place to put them.
        if self.wants(ResourceType::CHANNEL) {
            self.0.guild_channels.insert(guild.id, HashSet::new());
            self.cache_guild_channels(guild.id, guild.channels);
        }

        if self.wants(ResourceType::EMOJI) {
            self.0.guild_emojis.insert(guild.id, HashSet::new());
            self.cache_emojis(guild.id, guild.emojis);
        }

        if self.wants(ResourceType::MEMBER) {
            self.0.guild_members.insert(guild.id, HashSet::new());
            self.cache_members(guild.id, guild.members);
        }

        if self.wants(ResourceType::PRESENCE) {
            self.0.guild_presences.insert(guild.id, HashSet::new());
            self.cache_presences(
                guild.id,
                guild.presences.into_iter().map(CachedPresence::from),
            );
        }

        if self.wants(ResourceType::ROLE) {
            self.0.guild_roles.insert(guild.id, HashSet::new());
            self.cache_roles(guild.id, guild.roles);
        }

        if self.wants(ResourceType::VOICE_STATE) {
            self.0.voice_state_guilds.insert(guild.id, HashSet::new());
            self.cache_voice_states(guild.voice_states);
        }

        if self.wants(ResourceType::STAGE_INSTANCE) {
            self.0
                .guild_stage_instances
                .insert(guild.id, HashSet::new());
            self.cache_stage_instances(guild.id, guild.stage_instances);
        }

        let guild = CachedGuild {
            id: guild.id,
            afk_channel_id: guild.afk_channel_id,
            afk_timeout: guild.afk_timeout,
            application_id: guild.application_id,
            banner: guild.banner,
            default_message_notifications: guild.default_message_notifications,
            description: guild.description,
            discovery_splash: guild.discovery_splash,
            explicit_content_filter: guild.explicit_content_filter,
            features: guild.features,
            icon: guild.icon,
            joined_at: guild.joined_at,
            large: guild.large,
            max_members: guild.max_members,
            max_presences: guild.max_presences,
            member_count: guild.member_count,
            mfa_level: guild.mfa_level,
            name: guild.name,
            nsfw_level: guild.nsfw_level,
            owner: guild.owner,
            owner_id: guild.owner_id,
            permissions: guild.permissions,
            preferred_locale: guild.preferred_locale,
            premium_subscription_count: guild.premium_subscription_count,
            premium_tier: guild.premium_tier,
            rules_channel_id: guild.rules_channel_id,
            splash: guild.splash,
            system_channel_id: guild.system_channel_id,
            system_channel_flags: guild.system_channel_flags,
            unavailable: guild.unavailable,
            verification_level: guild.verification_level,
            vanity_url_code: guild.vanity_url_code,
            widget_channel_id: guild.widget_channel_id,
            widget_enabled: guild.widget_enabled,
        };

        self.0.unavailable_guilds.remove(&guild.id);
        self.0.guilds.insert(guild.id, guild);
    }

    fn cache_integration(&self, guild_id: GuildId, integration: GuildIntegration) {
        self.0
            .guild_integrations
            .entry(guild_id)
            .or_default()
            .insert(integration.id);

        upsert_guild_item(
            &self.0.integrations,
            guild_id,
            (guild_id, integration.id),
            integration,
        );
    }

    fn cache_member(&self, guild_id: GuildId, member: Member) {
        let member_id = member.user.id;
        let id = (guild_id, member_id);

        if let Some(m) = self.0.members.get(&id) {
            if *m == member {
                return;
            }
        }

        let user_id = member.user.id;

        self.cache_user(Cow::Owned(member.user), Some(guild_id));
        let cached = CachedMember {
            deaf: Some(member.deaf),
            guild_id,
            joined_at: member.joined_at,
            mute: Some(member.mute),
            nick: member.nick,
            pending: member.pending,
            premium_since: member.premium_since,
            roles: member.roles,
            user_id,
        };
        self.0.members.insert(id, cached);
        self.0
            .guild_members
            .entry(guild_id)
            .or_default()
            .insert(member_id);
    }

    fn cache_borrowed_partial_member(
        &self,
        guild_id: GuildId,
        member: &PartialMember,
        user_id: UserId,
    ) {
        let id = (guild_id, user_id);

        if let Some(m) = self.0.members.get(&id) {
            if *m == member {
                return;
            }
        }

        self.0
            .guild_members
            .entry(guild_id)
            .or_default()
            .insert(user_id);

        let cached = CachedMember {
            deaf: Some(member.deaf),
            guild_id,
            joined_at: member.joined_at.to_owned(),
            mute: Some(member.mute),
            nick: member.nick.to_owned(),
            pending: false,
            premium_since: None,
            roles: member.roles.to_owned(),
            user_id,
        };
        self.0.members.insert(id, cached);
    }

    fn cache_borrowed_interaction_member(&self, guild_id: GuildId, member: &InteractionMember) {
        let id = (guild_id, member.id);

        let (deaf, mute) = match self.0.members.get(&id) {
            Some(m) if *m == member => return,
            Some(m) => (m.deaf, m.mute),
            None => (None, None),
        };

        self.0
            .guild_members
            .entry(guild_id)
            .or_default()
            .insert(member.id);

        let cached = CachedMember {
            deaf,
            guild_id,
            joined_at: member.joined_at.to_owned(),
            mute,
            nick: member.nick.to_owned(),
            pending: false,
            premium_since: member.premium_since.to_owned(),
            roles: member.roles.to_owned(),
            user_id: member.id,
        };

        self.0.members.insert(id, cached);
    }

    fn cache_members(&self, guild_id: GuildId, members: impl IntoIterator<Item = Member>) {
        for member in members {
            self.cache_member(guild_id, member);
        }
    }

    fn cache_presences(
        &self,
        guild_id: GuildId,
        presences: impl IntoIterator<Item = CachedPresence>,
    ) {
        for presence in presences {
            self.cache_presence(guild_id, presence);
        }
    }

    fn cache_presence(&self, guild_id: GuildId, presence: CachedPresence) {
        self.0
            .presences
            .insert((guild_id, presence.user_id), presence);
    }

    fn cache_private_channel(&self, private_channel: PrivateChannel) {
        self.0
            .channels_private
            .insert(private_channel.id, private_channel);
    }

    fn cache_roles(&self, guild_id: GuildId, roles: impl IntoIterator<Item = Role>) {
        for role in roles {
            self.cache_role(guild_id, role);
        }
    }

    fn cache_role(&self, guild_id: GuildId, role: Role) {
        // Insert the role into the guild_roles map
        self.0
            .guild_roles
            .entry(guild_id)
            .or_default()
            .insert(role.id);

        // Insert the role into the all roles map
        upsert_guild_item(&self.0.roles, guild_id, role.id, role);
    }

    fn cache_stage_instances(
        &self,
        guild_id: GuildId,
        stage_instances: impl IntoIterator<Item = StageInstance>,
    ) {
        for stage_instance in stage_instances {
            self.cache_stage_instance(guild_id, stage_instance);
        }
    }

    fn cache_stage_instance(&self, guild_id: GuildId, stage_instance: StageInstance) {
        self.0
            .guild_stage_instances
            .entry(guild_id)
            .or_default()
            .insert(stage_instance.id);

        upsert_guild_item(
            &self.0.stage_instances,
            guild_id,
            stage_instance.id,
            stage_instance,
        );
    }

    fn cache_user(&self, user: Cow<'_, User>, guild_id: Option<GuildId>) {
        match self.0.users.get_mut(&user.id) {
            Some(mut u) if u.0 == *user => {
                if let Some(guild_id) = guild_id {
                    u.1.insert(guild_id);
                }

                return;
            }
            Some(_) | None => {}
        }
        let user = user.into_owned();

        if let Some(guild_id) = guild_id {
            let mut guild_id_set = BTreeSet::new();
            guild_id_set.insert(guild_id);
            self.0.users.insert(user.id, (user, guild_id_set));
        }
    }

    fn cache_voice_states(&self, voice_states: impl IntoIterator<Item = VoiceState>) {
        for voice_state in voice_states {
            self.cache_voice_state(voice_state);
        }
    }

    fn cache_voice_state(&self, voice_state: VoiceState) {
        // This should always exist, but just incase use a match
        let guild_id = match voice_state.guild_id {
            Some(id) => id,
            None => return,
        };

        let user_id = voice_state.user_id;

        // Check if the user is switching channels in the same guild (ie. they already have a voice state entry)
        if let Some(voice_state) = self.0.voice_states.get(&(guild_id, user_id)) {
            if let Some(channel_id) = voice_state.channel_id {
                let remove_channel_mapping = self
                    .0
                    .voice_state_channels
                    .get_mut(&channel_id)
                    .map(|mut channel_voice_states| {
                        channel_voice_states.remove(&(guild_id, user_id));

                        channel_voice_states.is_empty()
                    })
                    .unwrap_or_default();

                if remove_channel_mapping {
                    self.0.voice_state_channels.remove(&channel_id);
                }
            }
        }

        // Check if the voice channel_id does not exist, signifying that the user has left
        if voice_state.channel_id.is_none() {
            {
                let remove_guild = self
                    .0
                    .voice_state_guilds
                    .get_mut(&guild_id)
                    .map(|mut guild_users| {
                        guild_users.remove(&user_id);

                        guild_users.is_empty()
                    })
                    .unwrap_or_default();

                if remove_guild {
                    self.0.voice_state_guilds.remove(&guild_id);
                }
            }

            self.0.voice_states.remove(&(guild_id, user_id));

            return;
        }

        let maybe_channel_id = voice_state.channel_id;
        self.0.voice_states.insert((guild_id, user_id), voice_state);

        self.0
            .voice_state_guilds
            .entry(guild_id)
            .or_default()
            .insert(user_id);

        if let Some(channel_id) = maybe_channel_id {
            self.0
                .voice_state_channels
                .entry(channel_id)
                .or_default()
                .insert((guild_id, user_id));
        }
    }

    fn delete_group(&self, channel_id: ChannelId) {
        self.0.groups.remove(&channel_id);
    }

    fn unavailable_guild(&self, guild_id: GuildId) {
        self.0.unavailable_guilds.insert(guild_id);
        self.0.guilds.remove(&guild_id);
    }

    /// Delete a guild channel from the cache.
    ///
    /// The guild channel data itself and the channel entry in its guild's list
    /// of channels will be deleted.
    fn delete_guild_channel(&self, channel_id: ChannelId) {
        if let Some((_, item)) = self.0.channels_guild.remove(&channel_id) {
            if let Some(mut guild_channels) = self.0.guild_channels.get_mut(&item.guild_id) {
                guild_channels.remove(&channel_id);
            }
        }
    }

    fn delete_integration(&self, guild_id: GuildId, integration_id: IntegrationId) {
        if self
            .0
            .integrations
            .remove(&(guild_id, integration_id))
            .is_some()
        {
            if let Some(mut integrations) = self.0.guild_integrations.get_mut(&guild_id) {
                integrations.remove(&integration_id);
            }
        }
    }

    fn delete_role(&self, role_id: RoleId) {
        if let Some((_, role)) = self.0.roles.remove(&role_id) {
            if let Some(mut roles) = self.0.guild_roles.get_mut(&role.guild_id) {
                roles.remove(&role_id);
            }
        }
    }

    fn delete_stage_instance(&self, stage_id: StageId) {
        if let Some((_, data)) = self.0.stage_instances.remove(&stage_id) {
            let guild_id = data.guild_id;

            if let Some(mut stage_instances) = self.0.guild_stage_instances.get_mut(&guild_id) {
                stage_instances.remove(&stage_id);
            }
        }
    }

    /// Determine whether the configured cache wants a specific resource to be
    /// processed.
    fn wants(&self, resource_type: ResourceType) -> bool {
        self.0.config.resource_types().contains(resource_type)
    }
}

const fn presence_user_id(user_or_id: &UserOrId) -> UserId {
    match user_or_id {
        UserOrId::User(u) => u.id,
        UserOrId::UserId { id } => *id,
    }
}

#[cfg(test)]
mod tests {
    use crate::InMemoryCache;
    use std::borrow::Cow;
    use twilight_model::{
        channel::{
            stage_instance::PrivacyLevel, ChannelType, GuildChannel, StageInstance, TextChannel,
        },
        gateway::payload::{
            GuildEmojisUpdate, MemberRemove, RoleDelete, StageInstanceCreate, StageInstanceDelete,
            StageInstanceUpdate,
        },
        guild::{
            DefaultMessageNotificationLevel, Emoji, ExplicitContentFilter, Guild, Member, MfaLevel,
            NSFWLevel, Permissions, PremiumTier, Role, SystemChannelFlags, VerificationLevel,
        },
        id::{ChannelId, EmojiId, GuildId, RoleId, StageId, UserId},
        user::{CurrentUser, User},
        voice::VoiceState,
    };

    fn current_user(id: u64) -> CurrentUser {
        CurrentUser {
            avatar: None,
            bot: true,
            discriminator: "9876".to_owned(),
            email: None,
            id: UserId(id),
            mfa_enabled: true,
            name: "test".to_owned(),
            verified: Some(true),
            premium_type: None,
            public_flags: None,
            flags: None,
            locale: None,
        }
    }

    fn emoji(id: EmojiId, user: Option<User>) -> Emoji {
        Emoji {
            animated: false,
            available: true,
            id,
            managed: false,
            name: "test".to_owned(),
            require_colons: true,
            roles: Vec::new(),
            user,
        }
    }

    fn member(id: UserId, guild_id: GuildId) -> Member {
        Member {
            deaf: false,
            guild_id,
            hoisted_role: None,
            joined_at: None,
            mute: false,
            nick: None,
            pending: false,
            premium_since: None,
            roles: Vec::new(),
            user: user(id),
        }
    }

    fn role(id: RoleId) -> Role {
        Role {
            color: 0,
            hoist: false,
            id,
            managed: false,
            mentionable: false,
            name: "test".to_owned(),
            permissions: Permissions::empty(),
            position: 0,
            tags: None,
        }
    }

    fn user(id: UserId) -> User {
        User {
            avatar: None,
            bot: false,
            discriminator: "0001".to_owned(),
            email: None,
            flags: None,
            id,
            locale: None,
            mfa_enabled: None,
            name: "user".to_owned(),
            premium_type: None,
            public_flags: None,
            system: None,
            verified: None,
        }
    }

    fn voice_state(
        guild_id: GuildId,
        channel_id: Option<ChannelId>,
        user_id: UserId,
    ) -> VoiceState {
        VoiceState {
            channel_id,
            deaf: false,
            guild_id: Some(guild_id),
            member: None,
            mute: true,
            self_deaf: false,
            self_mute: true,
            self_stream: false,
            session_id: "a".to_owned(),
            suppress: false,
            token: None,
            user_id,
            request_to_speak_timestamp: Some("2021-04-21T22:16:50+0000".to_owned()),
        }
    }

    /// Test retrieval of the current user, notably that it doesn't simply
    /// panic or do anything funny. This is the only synchronous mutex that we
    /// might have trouble with across await points if we're not careful.
    #[test]
    fn test_current_user_retrieval() {
        let cache = InMemoryCache::new();
        assert!(cache.current_user().is_none());
        cache.cache_current_user(current_user(1));
        assert!(cache.current_user().is_some());
    }

    #[test]
    fn test_guild_create_channels_have_guild_ids() {
        let channels = Vec::from([GuildChannel::Text(TextChannel {
            id: ChannelId(111),
            guild_id: None,
            kind: ChannelType::GuildText,
            last_message_id: None,
            last_pin_timestamp: None,
            name: "guild channel with no guild id".to_owned(),
            nsfw: true,
            permission_overwrites: Vec::new(),
            parent_id: None,
            position: 1,
            rate_limit_per_user: None,
            topic: None,
        })]);

        let guild = Guild {
            id: GuildId(123),
            afk_channel_id: None,
            afk_timeout: 300,
            application_id: None,
            banner: None,
            channels,
            default_message_notifications: DefaultMessageNotificationLevel::Mentions,
            description: None,
            discovery_splash: None,
            emojis: Vec::new(),
            explicit_content_filter: ExplicitContentFilter::AllMembers,
            features: vec![],
            icon: None,
            joined_at: Some("".to_owned()),
            large: false,
            max_members: Some(50),
            max_presences: Some(100),
            member_count: Some(25),
            members: Vec::new(),
            mfa_level: MfaLevel::Elevated,
            name: "this is a guild".to_owned(),
            nsfw_level: NSFWLevel::AgeRestricted,
            owner: Some(false),
            owner_id: UserId(456),
            permissions: Some(Permissions::SEND_MESSAGES),
            preferred_locale: "en-GB".to_owned(),
            premium_subscription_count: Some(0),
            premium_tier: PremiumTier::None,
            presences: Vec::new(),
            roles: Vec::new(),
            splash: None,
            stage_instances: Vec::new(),
            system_channel_id: None,
            system_channel_flags: SystemChannelFlags::SUPPRESS_JOIN_NOTIFICATIONS,
            rules_channel_id: None,
            unavailable: false,
            verification_level: VerificationLevel::VeryHigh,
            voice_states: Vec::new(),
            vanity_url_code: None,
            widget_channel_id: None,
            widget_enabled: None,
            max_video_channel_users: None,
            approximate_member_count: None,
            approximate_presence_count: None,
        };

        let cache = InMemoryCache::new();
        cache.cache_guild(guild);

        let channel = cache.guild_channel(ChannelId(111)).unwrap();

        // The channel was given to the cache without a guild ID, but because
        // it's part of a guild create, the cache can automatically attach the
        // guild ID to it. So now, the channel's guild ID is present with the
        // correct value.
        match channel {
            GuildChannel::Text(ref c) => {
                assert_eq!(Some(GuildId(123)), c.guild_id);
            }
            _ => panic!("{:?}", channel),
        }
    }

    #[test]
    fn test_syntax_update() {
        let cache = InMemoryCache::new();
        cache.update(&RoleDelete {
            guild_id: GuildId(0),
            role_id: RoleId(1),
        });
    }

    #[test]
    fn test_cache_user_guild_state() {
        let user_id = UserId(2);
        let cache = InMemoryCache::new();
        cache.cache_user(Cow::Owned(user(user_id)), Some(GuildId(1)));

        // Test the guild's ID is the only one in the user's set of guilds.
        {
            let user = cache.0.users.get(&user_id).unwrap();
            assert!(user.1.contains(&GuildId(1)));
            assert_eq!(1, user.1.len());
        }

        // Test that a second guild will cause 2 in the set.
        cache.cache_user(Cow::Owned(user(user_id)), Some(GuildId(3)));

        {
            let user = cache.0.users.get(&user_id).unwrap();
            assert!(user.1.contains(&GuildId(3)));
            assert_eq!(2, user.1.len());
        }

        // Test that removing a user from a guild will cause the ID to be
        // removed from the set, leaving the other ID.
        cache.update(&MemberRemove {
            guild_id: GuildId(3),
            user: user(user_id),
        });

        {
            let user = cache.0.users.get(&user_id).unwrap();
            assert!(!user.1.contains(&GuildId(3)));
            assert_eq!(1, user.1.len());
        }

        // Test that removing the user from its last guild removes the user's
        // entry.
        cache.update(&MemberRemove {
            guild_id: GuildId(1),
            user: user(user_id),
        });
        assert!(!cache.0.users.contains_key(&user_id));
    }

    #[test]
    fn test_stage_channels() {
        let cache = InMemoryCache::new();

        let stage_instance = StageInstance {
            channel_id: ChannelId(1),
            discoverable_disabled: true,
            guild_id: GuildId(2),
            id: StageId(3),
            privacy_level: PrivacyLevel::GuildOnly,
            topic: "topic".into(),
        };

        cache.update(&StageInstanceCreate(stage_instance.clone()));

        {
            let cached_instances = cache
                .guild_stage_instances(stage_instance.guild_id)
                .unwrap();
            assert_eq!(1, cached_instances.len());
        }

        {
            let cached_instance = cache.stage_instance(stage_instance.id).unwrap();
            assert_eq!(stage_instance.topic, cached_instance.topic);
        }

        let new_stage_instance = StageInstance {
            topic: "a new topic".into(),
            ..stage_instance
        };

        cache.update(&StageInstanceUpdate(new_stage_instance.clone()));

        {
            let cached_instance = cache.stage_instance(stage_instance.id).unwrap();
            assert_ne!(stage_instance.topic, cached_instance.topic);
            assert_eq!(new_stage_instance.topic, "a new topic");
        }

        cache.update(&StageInstanceDelete(new_stage_instance));

        {
            let cached_instances = cache
                .guild_stage_instances(stage_instance.guild_id)
                .unwrap();
            assert_eq!(0, cached_instances.len());
        }

        {
            let cached_instance = cache.stage_instance(stage_instance.id);
            assert_eq!(cached_instance, None);
        }
    }

    #[test]
    fn test_voice_state_inserts_and_removes() {
        let cache = InMemoryCache::new();

        // Note: Channel ids are `<guildid><idx>` where idx is the index of the channel id
        // This is done to prevent channel id collisions between guilds
        // The other 2 ids are not special since they cant overlap

        // User 1 joins guild 1's channel 11 (1 channel, 1 guild)
        {
            // Ids for this insert
            let (guild_id, channel_id, user_id) = (GuildId(1), ChannelId(11), UserId(1));
            cache.cache_voice_state(voice_state(guild_id, Some(channel_id), user_id));

            // The new user should show up in the global voice states
            assert!(cache.0.voice_states.contains_key(&(guild_id, user_id)));
            // There should only be the one new voice state in there
            assert_eq!(1, cache.0.voice_states.len());

            // The new channel should show up in the voice states by channel lookup
            assert!(cache.0.voice_state_channels.contains_key(&channel_id));
            assert_eq!(1, cache.0.voice_state_channels.len());

            // The new guild should also show up in the voice states by guild lookup
            assert!(cache.0.voice_state_guilds.contains_key(&guild_id));
            assert_eq!(1, cache.0.voice_state_guilds.len());
        }

        // User 2 joins guild 2's channel 21 (2 channels, 2 guilds)
        {
            // Ids for this insert
            let (guild_id, channel_id, user_id) = (GuildId(2), ChannelId(21), UserId(2));
            cache.cache_voice_state(voice_state(guild_id, Some(channel_id), user_id));

            // The new voice state should show up in the global voice states
            assert!(cache.0.voice_states.contains_key(&(guild_id, user_id)));
            // There should be two voice states now that we have inserted another
            assert_eq!(2, cache.0.voice_states.len());

            // The new channel should also show up in the voice states by channel lookup
            assert!(cache.0.voice_state_channels.contains_key(&channel_id));
            assert_eq!(2, cache.0.voice_state_channels.len());

            // The new guild should also show up in the voice states by guild lookup
            assert!(cache.0.voice_state_guilds.contains_key(&guild_id));
            assert_eq!(2, cache.0.voice_state_guilds.len());
        }

        // User 3 joins guild 1's channel 12  (3 channels, 2 guilds)
        {
            // Ids for this insert
            let (guild_id, channel_id, user_id) = (GuildId(1), ChannelId(12), UserId(3));
            cache.cache_voice_state(voice_state(guild_id, Some(channel_id), user_id));

            // The new voice state should show up in the global voice states
            assert!(cache.0.voice_states.contains_key(&(guild_id, user_id)));
            assert_eq!(3, cache.0.voice_states.len());

            // The new channel should also show up in the voice states by channel lookup
            assert!(cache.0.voice_state_channels.contains_key(&channel_id));
            assert_eq!(3, cache.0.voice_state_channels.len());

            // The guild should still show up in the voice states by guild lookup
            assert!(cache.0.voice_state_guilds.contains_key(&guild_id));
            // Since we have used a guild that has been inserted into the cache already, there
            // should not be a new guild in the map
            assert_eq!(2, cache.0.voice_state_guilds.len());
        }

        // User 3 moves to guild 1's channel 11 (2 channels, 2 guilds)
        {
            // Ids for this insert
            let (guild_id, channel_id, user_id) = (GuildId(1), ChannelId(11), UserId(3));
            cache.cache_voice_state(voice_state(guild_id, Some(channel_id), user_id));

            // The new voice state should show up in the global voice states
            assert!(cache.0.voice_states.contains_key(&(guild_id, user_id)));
            // The amount of global voice states should not change since it was a move, not a join
            assert_eq!(3, cache.0.voice_states.len());

            // The new channel should show up in the voice states by channel lookup
            assert!(cache.0.voice_state_channels.contains_key(&channel_id));
            // The old channel should be removed from the lookup table
            assert_eq!(2, cache.0.voice_state_channels.len());

            // The guild should still show up in the voice states by guild lookup
            assert!(cache.0.voice_state_guilds.contains_key(&guild_id));
            assert_eq!(2, cache.0.voice_state_guilds.len());
        }

        // User 3 dcs (2 channels, 2 guilds)
        {
            let (guild_id, channel_id, user_id) = (GuildId(1), ChannelId(11), UserId(3));
            cache.cache_voice_state(voice_state(guild_id, None, user_id));

            // Now that the user left, they should not show up in the voice states
            assert!(!cache.0.voice_states.contains_key(&(guild_id, user_id)));
            assert_eq!(2, cache.0.voice_states.len());

            // Since they were not alone in their channel, the channel and guild mappings should not disappear
            assert!(cache.0.voice_state_channels.contains_key(&channel_id));
            // assert_eq!(2, cache.0.voice_state_channels.len());
            assert!(cache.0.voice_state_guilds.contains_key(&guild_id));
            assert_eq!(2, cache.0.voice_state_guilds.len());
        }

        // User 2 dcs (1 channel, 1 guild)
        {
            let (guild_id, channel_id, user_id) = (GuildId(2), ChannelId(21), UserId(2));
            cache.cache_voice_state(voice_state(guild_id, None, user_id));

            // Now that the user left, they should not show up in the voice states
            assert!(!cache.0.voice_states.contains_key(&(guild_id, user_id)));
            assert_eq!(1, cache.0.voice_states.len());

            // Since they were the last in their channel, the mapping should disappear
            assert!(!cache.0.voice_state_channels.contains_key(&channel_id));
            assert_eq!(1, cache.0.voice_state_channels.len());

            // Since they were the last in their guild, the mapping should disappear
            assert!(!cache.0.voice_state_guilds.contains_key(&guild_id));
            assert_eq!(1, cache.0.voice_state_guilds.len());
        }

        // User 1 dcs (0 channels, 0 guilds)
        {
            let (guild_id, _channel_id, user_id) = (GuildId(1), ChannelId(11), UserId(1));
            cache.cache_voice_state(voice_state(guild_id, None, user_id));

            // Since the last person has disconnected, the global voice states, guilds, and channels should all be gone
            assert!(cache.0.voice_states.is_empty());
            assert!(cache.0.voice_state_channels.is_empty());
            assert!(cache.0.voice_state_guilds.is_empty());
        }
    }

    #[test]
    fn test_voice_states() {
        let cache = InMemoryCache::new();
        cache.cache_voice_state(voice_state(GuildId(1), Some(ChannelId(2)), UserId(3)));
        cache.cache_voice_state(voice_state(GuildId(1), Some(ChannelId(2)), UserId(4)));

        // Returns both voice states for the channel that exists.
        assert_eq!(2, cache.voice_channel_states(ChannelId(2)).unwrap().len());

        // Returns None if the channel does not exist.
        assert!(cache.voice_channel_states(ChannelId(0)).is_none());
    }

    #[test]
    fn test_cache_role() {
        let cache = InMemoryCache::new();

        // Single inserts
        {
            // The role ids for the guild with id 1
            let guild_1_role_ids = (1..=10).map(RoleId).collect::<Vec<_>>();
            // Map the role ids to a test role
            let guild_1_roles = guild_1_role_ids
                .iter()
                .copied()
                .map(role)
                .collect::<Vec<_>>();
            // Cache all the roles using cache role
            for role in guild_1_roles.clone() {
                cache.cache_role(GuildId(1), role);
            }

            // Check for the cached guild role ids
            let cached_roles = cache.guild_roles(GuildId(1)).unwrap();
            assert_eq!(cached_roles.len(), guild_1_role_ids.len());
            assert!(guild_1_role_ids.iter().all(|id| cached_roles.contains(id)));

            // Check for the cached role
            assert!(guild_1_roles
                .into_iter()
                .all(|role| cache.role(role.id).expect("Role missing from cache") == role))
        }

        // Bulk inserts
        {
            // The role ids for the guild with id 2
            let guild_2_role_ids = (101..=110).map(RoleId).collect::<Vec<_>>();
            // Map the role ids to a test role
            let guild_2_roles = guild_2_role_ids
                .iter()
                .copied()
                .map(role)
                .collect::<Vec<_>>();
            // Cache all the roles using cache roles
            cache.cache_roles(GuildId(2), guild_2_roles.clone());

            // Check for the cached guild role ids
            let cached_roles = cache.guild_roles(GuildId(2)).unwrap();
            assert_eq!(cached_roles.len(), guild_2_role_ids.len());
            assert!(guild_2_role_ids.iter().all(|id| cached_roles.contains(id)));

            // Check for the cached role
            assert!(guild_2_roles
                .into_iter()
                .all(|role| cache.role(role.id).expect("Role missing from cache") == role))
        }
    }

    #[test]
    fn test_cache_guild_member() {
        let cache = InMemoryCache::new();

        // Single inserts
        {
            let guild_1_user_ids = (1..=10).map(UserId).collect::<Vec<_>>();
            let guild_1_members = guild_1_user_ids
                .iter()
                .copied()
                .map(|id| member(id, GuildId(1)))
                .collect::<Vec<_>>();

            for member in guild_1_members {
                cache.cache_member(GuildId(1), member);
            }

            // Check for the cached guild members ids
            let cached_roles = cache.guild_members(GuildId(1)).unwrap();
            assert_eq!(cached_roles.len(), guild_1_user_ids.len());
            assert!(guild_1_user_ids.iter().all(|id| cached_roles.contains(id)));

            // Check for the cached members
            assert!(guild_1_user_ids
                .iter()
                .all(|id| cache.member(GuildId(1), *id).is_some()));

            // Check for the cached users
            assert!(guild_1_user_ids.iter().all(|id| cache.user(*id).is_some()));
        }

        // Bulk inserts
        {
            let guild_2_user_ids = (1..=10).map(UserId).collect::<Vec<_>>();
            let guild_2_members = guild_2_user_ids
                .iter()
                .copied()
                .map(|id| member(id, GuildId(2)))
                .collect::<Vec<_>>();
            cache.cache_members(GuildId(2), guild_2_members);

            // Check for the cached guild members ids
            let cached_roles = cache.guild_members(GuildId(1)).unwrap();
            assert_eq!(cached_roles.len(), guild_2_user_ids.len());
            assert!(guild_2_user_ids.iter().all(|id| cached_roles.contains(id)));

            // Check for the cached members
            assert!(guild_2_user_ids
                .iter()
                .copied()
                .all(|id| cache.member(GuildId(1), id).is_some()));

            // Check for the cached users
            assert!(guild_2_user_ids.iter().all(|id| cache.user(*id).is_some()));
        }
    }

    #[test]
    fn test_cache_emoji() {
        let cache = InMemoryCache::new();

        // The user to do some of the inserts
        fn user_mod(id: EmojiId) -> Option<User> {
            if id.0 % 2 == 0 {
                // Only use user for half
                Some(user(UserId(1)))
            } else {
                None
            }
        }

        // Single inserts
        {
            let guild_1_emoji_ids = (1..=10).map(EmojiId).collect::<Vec<_>>();
            let guild_1_emoji = guild_1_emoji_ids
                .iter()
                .copied()
                .map(|id| emoji(id, user_mod(id)))
                .collect::<Vec<_>>();

            for emoji in guild_1_emoji {
                cache.cache_emoji(GuildId(1), emoji);
            }

            for id in guild_1_emoji_ids.iter().cloned() {
                let global_emoji = cache.emoji(id);
                assert!(global_emoji.is_some());
            }

            // Ensure the emoji has been added to the per-guild lookup map to prevent
            // issues like #551 from returning
            let guild_emojis = cache.guild_emojis(GuildId(1));
            assert!(guild_emojis.is_some());
            let guild_emojis = guild_emojis.unwrap();

            assert_eq!(guild_1_emoji_ids.len(), guild_emojis.len());
            assert!(guild_1_emoji_ids.iter().all(|id| guild_emojis.contains(id)));
        }

        // Bulk inserts
        {
            let guild_2_emoji_ids = (11..=20).map(EmojiId).collect::<Vec<_>>();
            let guild_2_emojis = guild_2_emoji_ids
                .iter()
                .copied()
                .map(|id| emoji(id, user_mod(id)))
                .collect::<Vec<_>>();
            cache.cache_emojis(GuildId(2), guild_2_emojis);

            for id in guild_2_emoji_ids.iter().cloned() {
                let global_emoji = cache.emoji(id);
                assert!(global_emoji.is_some());
            }

            let guild_emojis = cache.guild_emojis(GuildId(2));

            assert!(guild_emojis.is_some());
            let guild_emojis = guild_emojis.unwrap();
            assert_eq!(guild_2_emoji_ids.len(), guild_emojis.len());
            assert!(guild_2_emoji_ids.iter().all(|id| guild_emojis.contains(id)));
        }
    }

    #[test]
    fn test_clear() {
        let cache = InMemoryCache::new();
        cache.cache_emoji(GuildId(1), emoji(EmojiId(3), None));
        cache.cache_member(GuildId(2), member(UserId(4), GuildId(2)));
        cache.clear();
        assert!(cache.0.emojis.is_empty());
        assert!(cache.0.members.is_empty());
    }

    #[test]
    fn test_emoji_removal() {
        let cache = InMemoryCache::new();

        let guild_id = GuildId(1);

        let emote = emoji(EmojiId(1), None);
        let emote_2 = emoji(EmojiId(2), None);
        let emote_3 = emoji(EmojiId(3), None);

        cache.cache_emoji(guild_id, emote.clone());
        cache.cache_emoji(guild_id, emote_2.clone());
        cache.cache_emoji(guild_id, emote_3.clone());

        cache.update(&GuildEmojisUpdate {
            emojis: vec![emote.clone(), emote_3.clone()],
            guild_id,
        });

        assert_eq!(cache.0.emojis.len(), 2);
        assert_eq!(cache.0.guild_emojis.get(&guild_id).unwrap().len(), 2);
        assert!(cache.emoji(emote.id).is_some());
        assert!(cache.emoji(emote_2.id).is_none());
        assert!(cache.emoji(emote_3.id).is_some());

        cache.update(&GuildEmojisUpdate {
            emojis: vec![emote.clone()],
            guild_id,
        });

        assert_eq!(cache.0.emojis.len(), 1);
        assert_eq!(cache.0.guild_emojis.get(&guild_id).unwrap().len(), 1);
        assert!(cache.emoji(emote.id).is_some());
        assert!(cache.emoji(emote_2.id).is_none());

        let emote_4 = emoji(EmojiId(4), None);

        cache.update(&GuildEmojisUpdate {
            emojis: vec![emote_4.clone()],
            guild_id,
        });

        assert_eq!(cache.0.emojis.len(), 1);
        assert_eq!(cache.0.guild_emojis.get(&guild_id).unwrap().len(), 1);
        assert!(cache.emoji(emote_4.id).is_some());
        assert!(cache.emoji(emote.id).is_none());

        cache.update(&GuildEmojisUpdate {
            emojis: vec![],
            guild_id,
        });

        assert!(cache.0.emojis.is_empty());
        assert!(cache.0.guild_emojis.get(&guild_id).unwrap().is_empty());
    }
}
