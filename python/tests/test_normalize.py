"""Edge cases of the OTLP/JSON and legacy model conversion."""

from __future__ import annotations

from typing import Any

import pytest

import grow_jaeger as sdk
from _fixtures import TRACE_ID, body

SPAN_ID = "55bc22d6caba571a"


@pytest.mark.parametrize(
    ("value", "micros"),
    [
        ("1795034918142512222", 1795034918142512),
        (1500, 1),
        (2500.0, 2),
        (float("nan"), 0),
        (True, 0),
        ("-5", 0),
        (None, 0),
    ],
)
def test_nanos_to_micros(value: Any, micros: int) -> None:
    assert sdk.nanos_to_micros(value) == micros


@pytest.mark.parametrize(
    ("value", "expected"),
    [
        ({"stringValue": "s"}, "s"),
        ({"boolValue": False}, False),
        ({"intValue": "9007199254740993"}, 9007199254740993),
        ({"intValue": 7}, 7),
        ({"intValue": "x"}, "x"),
        ({"doubleValue": "1.25"}, 1.25),
        ({"doubleValue": "bad"}, "bad"),
        ({"bytesValue": "AQI="}, "AQI="),
        ({"arrayValue": {"values": [{"intValue": "1"}, {}, {"stringValue": "a"}]}}, [1, "a"]),
        ({"kvlistValue": {"values": [{"key": "k", "value": {"boolValue": True}}]}}, {"k": True}),
        ({}, None),
        ("nope", None),
    ],
)
def test_any_value(value: Any, expected: Any) -> None:
    assert sdk.any_value(value) == expected


def test_key_values_skips_bad_entries() -> None:
    assert sdk.key_values([{"key": "a", "value": {"stringValue": "x"}}, {"key": 1}, "x"]) == {
        "a": "x"
    }
    assert sdk.key_values(None) == {}


def test_otlp_enum_names_and_missing_service() -> None:
    data = {
        "resourceSpans": [
            {
                "scopeSpans": [
                    {
                        "spans": [
                            {
                                "traceId": TRACE_ID,
                                "spanId": SPAN_ID,
                                "parentSpanId": "0000000000000000",
                                "name": "x",
                                "kind": "SPAN_KIND_PRODUCER",
                                "status": {"code": "STATUS_CODE_ERROR", "message": "m"},
                            },
                            {
                                "traceId": TRACE_ID,
                                "spanId": "1" * 16,
                                "kind": 9,
                                "status": {"code": "STATUS_CODE_OK"},
                            },
                        ]
                    }
                ]
            },
            "junk",
        ]
    }
    (trace,) = sdk.otlp_to_traces(data)
    by_id = {s.span_id: s for s in trace.spans}
    first = by_id[SPAN_ID]
    assert first.parent_span_id is None
    assert first.kind == "producer" and first.service_name == "unknown_service"
    assert first.status == sdk.SpanStatus("error", "m")
    assert by_id["1" * 16].kind == "unspecified"
    assert by_id["1" * 16].status.code == "ok"
    assert sdk.otlp_to_traces(None) == []


def test_otlp_groups_several_traces() -> None:
    other = "2" * 32
    data = {
        "resourceSpans": [
            {
                "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": ""}}]},
                "scopeSpans": [
                    {
                        "spans": [
                            {"traceId": other, "spanId": "3" * 16, "name": "b"},
                            {"traceId": TRACE_ID, "spanId": "4" * 16, "name": "a"},
                            {"traceId": other, "spanId": "5" * 16, "name": "c"},
                        ]
                    }
                ],
            }
        ]
    }
    traces = sdk.otlp_to_traces(data)
    assert [t.trace_id for t in traces] == [other, TRACE_ID]
    assert len(traces[0].spans) == 2


def test_legacy_parent_and_link_rules() -> None:
    other = "85092f96d876e30b507cfd1361b0325e"
    data = {
        "data": [
            {
                "traceID": TRACE_ID,
                "processes": {"p1": {"serviceName": "svc"}},
                "spans": [
                    {
                        "traceID": TRACE_ID,
                        "spanID": "6" * 16,
                        "operationName": "follows",
                        "references": [
                            {"refType": "FOLLOWS_FROM", "traceID": TRACE_ID, "spanID": SPAN_ID},
                            {"refType": "FOLLOWS_FROM", "traceID": other, "spanID": "7" * 16},
                            {"refType": "CHILD_OF"},
                        ],
                        "startTime": 10,
                        "duration": 5,
                        "tags": [
                            {"key": "span.kind", "value": "weird"},
                            {"key": "otel.status_code", "value": "ERROR"},
                            {"key": "obj", "value": {"x": 1}},
                        ],
                        "logs": [{"timestamp": 12, "fields": [{"key": "msg", "value": "hi"}]}],
                        "processID": "missing",
                    }
                ],
            }
        ]
    }
    (trace,) = sdk.legacy_to_traces(data)
    (span,) = trace.spans
    assert span.parent_span_id == SPAN_ID  # first reference in the same trace
    assert [link.trace_id for link in span.links] == [other]
    assert span.kind == "unspecified"
    assert span.status == sdk.SpanStatus("error")  # no description
    assert span.service_name == "unknown_service"
    assert span.attributes == {}
    assert span.events[0].name == "log" and span.events[0].attributes == {"msg": "hi"}
    assert span.end_micros == 15
    assert sdk.legacy_to_traces("x") == []


def test_legacy_fixture_events() -> None:
    (trace,) = sdk.legacy_to_traces(body("getTraceLegacy.ok"))
    assert {s.service_name for s in trace.spans} == {"sdk-gw-86837b62", "sdk-wk-86837b62"}


def test_summaries_and_dependencies_odd_values() -> None:
    summaries = sdk.summaries_from_body(
        {"summaries": [{"traceId": TRACE_ID, "spanCount": "x", "rootServiceName": ""}]}
    )
    assert summaries == [sdk.TraceSummary(TRACE_ID, 0, 0, 0)]
    assert sdk.summaries_from_body([]) == []
    assert sdk.dependencies_from_body(
        {"data": [{"parent": "a", "child": "b", "callCount": 3}]}
    ) == [sdk.DependencyLink("a", "b", 3)]
