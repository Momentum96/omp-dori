// Verbatim copy of dori-shared node/discord-watch/route.ts at ba2ba199; edit the source, then copy it again.
/**
 * Shared mention routing for the two Dori bots. Pure and dependency-free so the omp receiver can copy it verbatim;
 * the behavior of record is `../routing-vectors.json`, read by both receivers' tests.
 */

export type Bot = 'omo' | 'omp'
export const BOTS: readonly Bot[] = ['omo', 'omp']

export type RouteConfig = {
  readonly ownerId: string
  readonly topicChannels: readonly string[]
  readonly testChannels: readonly string[]
  readonly testOwnerId: string | null
  readonly bots: Readonly<Record<Bot, { readonly userId: string; readonly roleId: string | null }>>
  readonly legacyBotOwners: Readonly<Record<string, Bot>>
  readonly burstMs: number
}

export type RouteMessage = {
  readonly id: string
  readonly channel_id: string
  readonly author?: { readonly id?: string; readonly bot?: boolean }
  readonly content?: string
  readonly type?: unknown
  readonly flags?: number
  readonly webhook_id?: string | null
  readonly mentions?: readonly { readonly id: string }[]
  readonly mention_roles?: readonly string[]
  readonly message_reference?: { readonly type?: number; readonly message_id?: string; readonly channel_id?: string } | null
  readonly referenced_message?: { readonly author?: { readonly id?: string } } | null
}

/** A thread whose parent was resolved by the receiver; `starterAuthorId` is the author of the parent message with the thread's id. */
export type ThreadInfo = {
  readonly parentId: string | null
  readonly ownerId: string | null
  readonly starterAuthorId: string | null
}

export type Neighbor = { readonly id: string; readonly addressed: readonly Bot[]; readonly reason: Reason }

export type RouteContext = {
  readonly thread?: ThreadInfo
  readonly assignee?: Bot | null
  readonly neighbors?: readonly Neighbor[]
  /** Epoch ms; omitted means the whole history is known and every burst decision is final. */
  readonly now?: number
}

export type Reason =
  | 'ineligible-webhook' | 'ineligible-type' | 'ineligible-author' | 'ineligible-channel'
  | 'explicit' | 'reply' | 'thread-owner' | 'thread-starter' | 'thread-assignee'
  | 'burst' | 'burst-pending' | 'unaddressed'

export type RouteResult = {
  readonly addressed: readonly Bot[]
  readonly reason: Reason
  readonly assignee?: Bot | null
  readonly decideAt?: number
}

const DISCORD_EPOCH = 1420070400000n
export const snowflakeMs = (id: string): number => Number((BigInt(id) >> 22n) + DISCORD_EPOCH)
export const snowflakeAt = (ms: number): string => String((BigInt(Math.floor(ms)) - DISCORD_EPOCH) << 22n)

const ROUTED_TYPES = new Set([0, 19])
const FORWARD = 1
const ROUTED: ReadonlySet<Reason> = new Set(['explicit', 'reply', 'thread-owner', 'thread-starter', 'thread-assignee'])

const orderBots = (set: ReadonlySet<Bot>): Bot[] => BOTS.filter(bot => set.has(bot))

/** The test owner is a bot account; it counts as the owner only in the test channel and the test channel's threads. */
const authorIsOwner = (author: RouteMessage['author'], inTestScope: boolean, config: RouteConfig): boolean => {
  if (!author?.id) return false
  if (author.id === config.ownerId) return author.bot !== true
  return inTestScope && config.testOwnerId !== null && author.id === config.testOwnerId
}

const botOf = (userId: string | null | undefined, effectiveOwner: string | null, config: RouteConfig): Bot | null => {
  if (!userId || userId === effectiveOwner) return null
  const current = BOTS.find(bot => config.bots[bot].userId === userId)
  return current ?? config.legacyBotOwners[userId] ?? null
}

const explicitBots = (message: RouteMessage, config: RouteConfig): Set<Bot> => {
  const mentioned = new Set((message.mentions ?? []).map(user => user.id))
  const roles = new Set(message.mention_roles ?? [])
  const content = message.content ?? ''
  return new Set(BOTS.filter(bot => {
    const { userId, roleId } = config.bots[bot]
    const token = content.includes(`<@${userId}>`) || content.includes(`<@!${userId}>`)
    return (mentioned.has(userId) && token) || (roleId !== null && roles.has(roleId))
  }))
}

const replyBot = (message: RouteMessage, config: RouteConfig): Bot | null => {
  const reference = message.message_reference
  if (!reference || reference.type === FORWARD) return null
  const author = message.referenced_message?.author?.id
  return author ? (BOTS.find(bot => config.bots[bot].userId === author) ?? config.legacyBotOwners[author] ?? null) : null
}

const burst = (message: RouteMessage, context: RouteContext, config: RouteConfig): RouteResult => {
  const at = snowflakeMs(message.id)
  const candidates = (context.neighbors ?? [])
    .filter(n => n.id !== message.id && n.addressed.length > 0 && ROUTED.has(n.reason))
    .map(n => ({ n, at: snowflakeMs(n.id) }))
    .filter(c => Math.abs(c.at - at) <= config.burstMs)
  const decideAt = at + config.burstMs
  const decided = (context.now ?? Number.POSITIVE_INFINITY) >= decideAt || candidates.some(c => c.at >= at)
  if (!decided) return { addressed: [], reason: 'burst-pending', decideAt }
  const nearest = candidates.sort((a, b) => Math.abs(a.at - at) - Math.abs(b.at - at) || a.at - b.at)[0]
  return nearest ? { addressed: [...nearest.n.addressed], reason: 'burst' } : { addressed: [], reason: 'unaddressed' }
}

export function route(message: RouteMessage, context: RouteContext, config: RouteConfig): RouteResult {
  if (message.webhook_id) return { addressed: [], reason: 'ineligible-webhook' }
  if (typeof message.type !== 'number' || !ROUTED_TYPES.has(message.type)) return { addressed: [], reason: 'ineligible-type' }
  const top = message.channel_id
  const parent = context.thread?.parentId ?? null
  const home = config.topicChannels.includes(top) || config.testChannels.includes(top) ? top
    : parent !== null && (config.topicChannels.includes(parent) || config.testChannels.includes(parent)) ? parent : null
  if (home === null) return { addressed: [], reason: 'ineligible-channel' }
  const inTestScope = config.testChannels.includes(home)
  if (!authorIsOwner(message.author, inTestScope, config)) return { addressed: [], reason: 'ineligible-author' }
  const effectiveOwner = inTestScope ? config.testOwnerId : null

  const explicit = explicitBots(message, config)
  const reply = replyBot(message, config)
  const chosen: Set<Bot> = explicit.size > 0 ? explicit : reply ? new Set([reply]) : new Set()
  const thread = home === top ? undefined : context.thread
  const threadBot = thread ? botOf(thread.ownerId, effectiveOwner, config) : null
  const starterBot = thread && !threadBot ? botOf(thread.starterAuthorId, effectiveOwner, config) : null
  const ownerThread = thread !== undefined && threadBot === null && starterBot === null
  const previous = context.assignee ?? null
  const assignee = !ownerThread ? undefined : chosen.size === 1 ? [...chosen][0]! : chosen.size === 2 ? null : previous
  const withAssignee = (result: RouteResult): RouteResult => assignee === undefined ? result : { ...result, assignee }

  if (explicit.size > 0) return withAssignee({ addressed: orderBots(explicit), reason: 'explicit' })
  if (reply) return withAssignee({ addressed: [reply], reason: 'reply' })
  if (threadBot) return { addressed: [threadBot], reason: 'thread-owner' }
  if (starterBot) return { addressed: [starterBot], reason: 'thread-starter' }
  if (ownerThread && previous) return withAssignee({ addressed: [previous], reason: 'thread-assignee' })
  return withAssignee(burst(message, context, config))
}

const BURST_MS = 10_000
const snowflake = (value: unknown, field: string): string => {
  if (typeof value !== 'string' || !/^\d{1,20}$/.test(value)) throw new Error(`topology ${field} must be a snowflake string`)
  return value
}
const nullableSnowflake = (value: unknown, field: string): string | null => value === null || value === undefined ? null : snowflake(value, field)
const record = (value: unknown, field: string): Record<string, unknown> => {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new Error(`topology ${field} must be an object`)
  return value as Record<string, unknown>
}

export function routeConfigFromTopology(raw: unknown): RouteConfig {
  const t = record(raw, 'root')
  const topics = record(t['topicChannels'], 'topicChannels')
  const bots = record(t['bots'], 'bots')
  const testOwner = t['testOwner'] === null || t['testOwner'] === undefined ? {} : record(t['testOwner'], 'testOwner')
  const testChannels = [...new Set([nullableSnowflake(t['testChannel'], 'testChannel'), nullableSnowflake(testOwner['channelId'], 'testOwner.channelId')]
    .filter((id): id is string => id !== null))]
  const botEntry = (bot: Bot) => {
    const entry = record(bots[bot], `bots.${bot}`)
    return { userId: snowflake(entry['userId'], `bots.${bot}.userId`), roleId: nullableSnowflake(entry['roleId'], `bots.${bot}.roleId`) }
  }
  const legacy: Record<string, Bot> = {}
  for (const [id, bot] of Object.entries(record(t['legacyBotOwners'] ?? {}, 'legacyBotOwners'))) {
    if (bot !== 'omo' && bot !== 'omp') throw new Error(`topology legacyBotOwners.${id} must be omo or omp`)
    legacy[snowflake(id, 'legacyBotOwners key')] = bot
  }
  const burstMs = t['burstMs'] === undefined ? BURST_MS : t['burstMs']
  if (typeof burstMs !== 'number' || !Number.isFinite(burstMs) || burstMs < 0) throw new Error('topology burstMs must be a non-negative number')
  return {
    ownerId: snowflake(t['ownerId'], 'ownerId'),
    topicChannels: Object.entries(topics).map(([name, id]) => snowflake(id, `topicChannels.${name}`)),
    testChannels,
    testOwnerId: nullableSnowflake(testOwner['userId'], 'testOwner.userId'),
    bots: { omo: botEntry('omo'), omp: botEntry('omp') },
    legacyBotOwners: legacy,
    burstMs,
  }
}
