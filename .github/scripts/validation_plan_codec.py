#!/usr/bin/env python3
"""Bounded canonical JSON codec for the Validation plan contract."""

import json

from validation_contracts import MAX_JSON_INTEGER, ContractError
from validation_plan_contract import (
    MAX_PLAN_INPUT_BYTES as _MAX_PLAN_INPUT_BYTES,
    MAX_PLAN_ITEMS as _MAX_PLAN_ITEMS,
    MAX_PLAN_OUTPUT_BYTES as _MAX_PLAN_OUTPUT_BYTES,
    MAX_PLAN_TEXT_BYTES as _MAX_PLAN_TEXT_BYTES,
    ValidationPlan,
    _reject_constant,
    _reject_duplicate,
    plan_from_dict,
    plan_to_dict,
    validate_plan,
)


MAX_PLAN_INPUT_BYTES = _MAX_PLAN_INPUT_BYTES
MAX_PLAN_ITEMS = _MAX_PLAN_ITEMS
MAX_PLAN_OUTPUT_BYTES = _MAX_PLAN_OUTPUT_BYTES
MAX_PLAN_TEXT_BYTES = _MAX_PLAN_TEXT_BYTES


def validate_plan_budgets(plan: ValidationPlan) -> None:
    validate_plan(plan)


def _serialize_payload(payload: dict[str, object], name: str) -> str:
    try:
        text = (
            json.dumps(
                payload,
                ensure_ascii=False,
                allow_nan=False,
                sort_keys=True,
                indent=2,
            )
            + "\n"
        )
        size = len(text.encode("utf-8"))
    except (RecursionError, TypeError, UnicodeEncodeError, ValueError) as error:
        raise ContractError(f"{name} cannot be canonically serialized") from error
    if size > MAX_PLAN_OUTPUT_BYTES:
        raise ContractError(f"{name} exceeds its serialized byte budget")
    return text


def serialize_plan(plan: ValidationPlan) -> str:
    validate_plan_budgets(plan)
    return _serialize_payload(plan_to_dict(plan), "Validation plan")


def _parse_int(value: str) -> int:
    sign = value.startswith("-")
    digits = value[1:] if sign else value
    if len(digits) > 19 or (len(digits) == 19 and digits > str(MAX_JSON_INTEGER)):
        raise ContractError("JSON integer exceeds its bounded range")
    return int(value)


def _input_text(value: object) -> str:
    if isinstance(value, bytes):
        if len(value) > MAX_PLAN_INPUT_BYTES:
            raise ContractError("Validation plan exceeds its input byte budget")
        try:
            text = value.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ContractError("Validation plan JSON must be valid UTF-8") from error
    elif isinstance(value, str):
        text = value
    else:
        raise ContractError("Validation plan JSON must be text or UTF-8 bytes")
    try:
        size = len(text.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ContractError("Validation plan JSON must be valid UTF-8") from error
    if size > MAX_PLAN_INPUT_BYTES:
        raise ContractError("Validation plan exceeds its input byte budget")
    return text


def parse_plan(value: object) -> ValidationPlan:
    text = _input_text(value)
    try:
        payload = json.loads(
            text,
            object_pairs_hook=_reject_duplicate,
            parse_constant=_reject_constant,
            parse_int=_parse_int,
        )
    except ContractError:
        raise
    except (
        json.JSONDecodeError,
        RecursionError,
        TypeError,
        UnicodeDecodeError,
        ValueError,
    ) as error:
        raise ContractError(f"invalid Validation plan JSON: {error}") from error
    plan = plan_from_dict(payload)
    if serialize_plan(plan) != text:
        raise ContractError("Validation plan is not canonically serialized")
    return plan
