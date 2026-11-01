import { request } from "./client";
import { ApiError } from "./errors";
import type { Account, InstanceEntity } from "./entities";

/** `GET /api/v2/instance` — anonymous; instance metadata + limits. */
export function getInstance(): Promise<InstanceEntity> {
  return request<InstanceEntity>({ path: "/api/v2/instance", skipAuth: true });
}

/** `GET /api/v1/instance` — the v1 shape carries `stats` + `email`. */
export interface InstanceV1Entity {
  stats?: {
    user_count?: number;
    status_count?: number;
    domain_count?: number;
  };
  email?: string | null;
  contact_account?: Account | null;
  [key: string]: unknown;
}

export function getInstanceV1(): Promise<InstanceV1Entity> {
  return request<InstanceV1Entity>({ path: "/instance", skipAuth: true });
}

export interface ExtendedDescription {
  updated_at?: string;
  content?: string;
}

/**
 * `GET /api/v1/instance/extended_description` — the long-form about
 * text; the endpoint is 404 (Mastodon semantics) when the admin left
 * it unset, which the /about page treats as "no section", not an
 * error. Any other failure degrades the same way.
 */
export async function getExtendedDescription(): Promise<ExtendedDescription | null> {
  try {
    return await request<ExtendedDescription>({
      path: "/instance/extended_description",
      skipAuth: true,
      silent: true,
    });
  } catch {
    return null;
  }
}

export interface PublicDomainBlock {
  domain: string;
  severity: string;
  comment: string;
}

/**
 * `GET /api/v1/instance/domain_blocks` — the public moderated-servers
 * listing; 404 when `instance.expose_moderated_instances` is off, so
 * the about page hides the section instead of failing.
 */
export async function getInstanceDomainBlocks(): Promise<
  PublicDomainBlock[] | null
> {
  try {
    return await request<PublicDomainBlock[]>({
      path: "/instance/domain_blocks",
      skipAuth: true,
      silent: true,
    });
  } catch (e) {
    if (e instanceof ApiError && e.status !== 404) throw e;
    return null;
  }
}

export interface ComposeLimits {
  maxCharacters: number;
  maxMediaAttachments: number;
  poll: {
    maxOptions: number;
    maxCharactersPerOption: number;
    minExpiration: number;
    maxExpiration: number;
  };
}

const DEFAULTS: ComposeLimits = {
  maxCharacters: 5000,
  maxMediaAttachments: 4,
  poll: {
    maxOptions: 4,
    maxCharactersPerOption: 80,
    minExpiration: 300,
    maxExpiration: 2629746,
  },
};

/** Resolve the compose limits, falling back when fields are absent. */
export function composeLimits(instance: InstanceEntity | null): ComposeLimits {
  const statuses = instance?.configuration?.statuses;
  const polls = instance?.configuration?.polls;
  return {
    maxCharacters: statuses?.max_characters ?? DEFAULTS.maxCharacters,
    maxMediaAttachments:
      statuses?.max_media_attachments ?? DEFAULTS.maxMediaAttachments,
    poll: {
      maxOptions: polls?.max_options ?? DEFAULTS.poll.maxOptions,
      maxCharactersPerOption:
        polls?.max_characters_per_option ??
        DEFAULTS.poll.maxCharactersPerOption,
      minExpiration: polls?.min_expiration ?? DEFAULTS.poll.minExpiration,
      maxExpiration: polls?.max_expiration ?? DEFAULTS.poll.maxExpiration,
    },
  };
}
