# generated by datamodel-codegen:
#   filename:  release-index-schema.json

from __future__ import annotations

from datetime import date
from typing import List, Optional

from pydantic import BaseModel, ConfigDict, RootModel


class Version(BaseModel):
    model_config = ConfigDict(
        extra='forbid',
    )
    version: str
    name: str
    release_notes_ready: Optional[bool] = None
    subnets: Optional[List[str]] = None


class Stage(BaseModel):
    model_config = ConfigDict(
        extra='forbid',
    )
    subnets: Optional[List[str]] = None
    bake_time: Optional[str] = None
    update_unassigned_nodes: Optional[bool] = None
    wait_for_next_week: Optional[bool] = None


class Release(BaseModel):
    model_config = ConfigDict(
        extra='forbid',
    )
    rc_name: str
    versions: List[Version]


class Rollout(BaseModel):
    model_config = ConfigDict(
        extra='forbid',
    )
    pause: Optional[bool] = None
    skip_days: Optional[List[date]] = None
    stages: List[Stage]


class ReleaseIndex(BaseModel):
    model_config = ConfigDict(
        extra='forbid',
    )
    rollout: Rollout
    releases: List[Release]


class Model(RootModel[ReleaseIndex]):
    root: ReleaseIndex
