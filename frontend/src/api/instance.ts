import { request } from "./client";
import type { InstanceEntity } from "./entities";

/** `GET /api/v2/instance` — anonymous; instance metadata + limits. */
export function getInstance(): Promise<InstanceEntity> {
  return request<InstanceEntity>({ path: "/api/v2/instance", skipAuth: true });
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
