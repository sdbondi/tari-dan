import type { Command } from "./Command";
import type { Epoch } from "./Epoch";
import type { ExtraData } from "./ExtraData";
import type { NodeHeight } from "./NodeHeight";
import type { QuorumCertificate } from "./QuorumCertificate";
import type { Shard } from "./Shard";
import type { ShardGroup } from "./ShardGroup";
export interface Block {
    id: string;
    network: string;
    parent: string;
    justify: QuorumCertificate;
    height: NodeHeight;
    epoch: Epoch;
    shard_group: ShardGroup;
    proposed_by: string;
    total_leader_fee: number;
    merkle_root: string;
    commands: Array<Command>;
    is_dummy: boolean;
    is_justified: boolean;
    is_committed: boolean;
    foreign_indexes: Record<Shard, bigint>;
    stored_at: Array<number> | null;
    signature: {
        public_nonce: string;
        signature: string;
    } | null;
    block_time: number | null;
    timestamp: number;
    base_layer_block_height: number;
    base_layer_block_hash: string;
    extra_data: ExtraData | null;
}