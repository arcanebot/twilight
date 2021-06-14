use super::InteractionType;

use crate::{
    channel::Message,
    guild::PartialMember,
    id::{ChannelId, GuildId, InteractionId},
};

use serde::{Deserialize, Serialize};

/// Data present in an [`Interaction`] of type [`MessageComponent`].
///
/// [`Interaction`]: super::Interaction
/// [`MessageComponent`]: super::Interaction::MessageComponent
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename(serialize = "Interaction"))]
pub struct MessageComponent {
    /// ID of the interaction.
    pub id: InteractionId,
    #[serde(rename = "type")]
    /// Kind of the interaction.
    pub kind: InteractionType,
    /// Token of the interaction.
    pub token: String,

    /// Present when the command is used in a guild.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<PartialMember>,
    /// Message the component is attached to
    pub message: Option<Message>,
    /// ID of the guild the interaction was triggered from.
    pub guild_id: Option<GuildId>,
    /// The channel the interaction was triggered from.
    pub channel_id: ChannelId,
    /// Stuff
    pub data: MessageComponentData,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MessageComponentData {
    /// Custom ID of the button
    pub custom_id: String,
    /// Type of component
    pub component_type: u8,
}
