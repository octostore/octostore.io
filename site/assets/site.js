(() => {
  const copyText = async (button, text) => {
    const original = button.textContent;
    try {
      await navigator.clipboard.writeText(text.trim());
      button.textContent = "Copied";
    } catch {
      button.textContent = "Select to copy";
    }
    window.setTimeout(() => {
      button.textContent = original;
    }, 1800);
  };

  for (const button of document.querySelectorAll("[data-copy-target]")) {
    button.addEventListener("click", async () => {
      const target = document.querySelector(button.dataset.copyTarget);
      if (!target) return;
      const text = "value" in target ? target.value : target.textContent;
      await copyText(button, text);
      if (button.textContent === "Select to copy") {
        target.focus?.();
        target.select?.();
      }
    });
  }

  for (const [index, nav] of [...document.querySelectorAll(".site-nav")].entries()) {
    const links = nav.querySelector(".nav-links");
    if (!links) continue;

    const menuId = links.id || `primary-menu-${index + 1}`;
    links.id = menuId;
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "nav-toggle";
    toggle.setAttribute("aria-controls", menuId);
    toggle.setAttribute("aria-expanded", "false");
    toggle.textContent = "Menu";
    nav.insertBefore(toggle, links);
    nav.classList.add("nav-enhanced");
    nav.dataset.navOpen = "false";

    const closeMenu = () => {
      nav.dataset.navOpen = "false";
      toggle.setAttribute("aria-expanded", "false");
      toggle.textContent = "Menu";
    };

    toggle.addEventListener("click", () => {
      const open = nav.dataset.navOpen !== "true";
      nav.dataset.navOpen = open.toString();
      toggle.setAttribute("aria-expanded", open.toString());
      toggle.textContent = open ? "Close" : "Menu";
    });
    links.addEventListener("click", (event) => {
      if (event.target.closest("a")) closeMenu();
    });
    nav.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && nav.dataset.navOpen === "true") {
        closeMenu();
        toggle.focus();
      }
    });
  }

  const year = document.querySelector("[data-current-year]");
  if (year) year.textContent = new Date().getFullYear().toString();

  const demo = document.querySelector("[data-election-demo]");
  if (!demo) return;

  const runButtons = [...document.querySelectorAll("[data-run-election]")];
  const roomLabel = demo.querySelector("[data-election-room]");
  const status = demo.querySelector("[data-race-status]");
  const handoff = demo.querySelector("[data-agent-handoff]");
  const instructionFields = [...demo.querySelectorAll("[data-agent-instruction]")];
  const candidates = [...demo.querySelectorAll("[data-candidate]")];
  const apiBase = demo.dataset.apiBase || "https://api.octostore.io";
  let electionStream;
  let pollingTimer;
  let observationTimer;
  let observationGeneration = 0;

  function instruction(electionId, candidateId) {
    return `Read https://octostore.io/agents/SKILL.md. Coordinate the merge-coordinator role in election ${electionId} using candidate ID ${candidateId}. Run your worker under the reference supervisor or an equivalent fail-closed host. Do not enter the merge gate until the supervisor reports leadership. Stop protected work if authority is lost or uncertain.`;
  }

  const setCandidate = (row, state, label) => {
    row.dataset.state = state;
    row.querySelector("[data-candidate-state]").textContent = label;
  };

  const stopObservation = () => {
    electionStream?.close();
    electionStream = undefined;
    window.clearInterval(pollingTimer);
    window.clearTimeout(observationTimer);
    pollingTimer = undefined;
    observationTimer = undefined;
  };

  const isCurrentGeneration = (generation) => generation === observationGeneration;

  const beginGeneration = () => {
    observationGeneration += 1;
    stopObservation();
    return observationGeneration;
  };

  const markUnconfirmed = (message) => {
    candidates.forEach((row) => setCandidate(row, "unconfirmed", "UNCONFIRMED"));
    status.textContent = message;
  };

  const endObservation = (generation) => {
    if (!isCurrentGeneration(generation)) return;
    stopObservation();
    observationGeneration += 1;
    markUnconfirmed(
      "Live observation ended after 15 minutes. This page no longer confirms the room state; agents must treat authority as not owned.",
    );
  };

  const isRecord = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  const isNonNegativeInteger = (value) => Number.isSafeInteger(value) && value >= 0;
  const isRfc3339DateTime = (value) => {
    if (typeof value !== "string") return false;
    const match = /^(\d{4})-(\d{2})-(\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(?:[Zz]|[+-](\d{2}):(\d{2}))$/.exec(value);
    if (!match) return false;

    const [, yearText, monthText, dayText, hourText, minuteText, secondText,
      offsetHourText, offsetMinuteText] = match;
    const year = Number(yearText);
    const month = Number(monthText);
    const day = Number(dayText);
    const hour = Number(hourText);
    const minute = Number(minuteText);
    const second = Number(secondText);
    const leapYear = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
    const daysInMonth = [31, leapYear ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    if (month < 1 || month > 12 || day < 1 || day > daysInMonth[month - 1] ||
        hour > 23 || minute > 59 || second > 59) {
      return false;
    }
    if (offsetHourText !== undefined &&
        (Number(offsetHourText) > 23 || Number(offsetMinuteText) > 59)) {
      return false;
    }
    return true;
  };

  const validateCreateResponse = (room) => {
    if (!isRecord(room) || typeof room.election_id !== "string" ||
        !/^[A-Za-z0-9_-]{8,64}$/.test(room.election_id)) {
      throw new Error("invalid election create response");
    }
    const basePath = `/elections/${room.election_id}`;
    if (room.campaign_path !== `${basePath}/campaign` || room.status_path !== basePath ||
        room.watch_path !== `${basePath}/watch`) {
      throw new Error("invalid election create response");
    }
    return room;
  };

  const validateLeader = (leader) => {
    if (!isRecord(leader) || typeof leader.candidate_id !== "string" ||
        !/^[A-Za-z0-9._:@-]{1,64}$/.test(leader.candidate_id) ||
        !Number.isSafeInteger(leader.term) || leader.term < 1 || !isRfc3339DateTime(leader.expires_at) ||
        !(leader.metadata === undefined || leader.metadata === null ||
          (typeof leader.metadata === "string" && leader.metadata.length <= 1024))) {
      throw new Error("invalid election leader response");
    }
    return leader;
  };

  const validateElectionState = (state, electionId, fromStream = false) => {
    if (!isRecord(state) || state.election_id !== electionId ||
        !["leader", "vacant"].includes(state.status) ||
        !isNonNegativeInteger(state.retry_after_ms)) {
      throw new Error("invalid election status response");
    }
    if (state.status === "leader") {
      validateLeader(state.leader);
    } else if (!(state.leader === undefined || state.leader === null)) {
      throw new Error("invalid vacant election response");
    }
    if (fromStream && (state.schema_version !== 1 || !isRfc3339DateTime(state.observed_at))) {
      throw new Error("invalid election stream response");
    }
    return state;
  };

  const requestJson = async (url, options) => {
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 12000);
    try {
      const response = await fetch(url, { ...options, signal: controller.signal });
      const requestId = response.headers.get("x-request-id");
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}${requestId ? ` · request ${requestId}` : ""}`);
      }
      return await response.json();
    } finally {
      window.clearTimeout(timeout);
    }
  };

  const renderElectionState = (state, generation, electionId, fromStream = false) => {
    if (!isCurrentGeneration(generation)) return;
    validateElectionState(state, electionId, fromStream);
    if (state.status === "vacant") {
      candidates.forEach((row) => setCandidate(row, "shared", "ROOM SHARED"));
      status.textContent = "The real room is live and vacant. Copy one instruction to each agent; this page will observe the current leader.";
      return;
    }

    for (const row of candidates) {
      if (row.dataset.candidate === state.leader.candidate_id) {
        setCandidate(row, "leader", `LEADER · TERM ${state.leader.term}`);
      } else {
        setCandidate(row, "follower", `WAIT FOR ${state.leader.candidate_id}`);
      }
    }
    status.textContent = `${state.leader.candidate_id} holds term ${state.leader.term}. Work still runs in the agent host; this page only observes election state.`;
  };

  const pollElection = (statusPath, electionId, generation) => {
    const read = async () => {
      if (!isCurrentGeneration(generation)) return;
      try {
        const state = await requestJson(`${apiBase}${statusPath}`, { method: "GET" });
        renderElectionState(state, generation, electionId);
      } catch (error) {
        if (!isCurrentGeneration(generation)) return;
        const detail = error.name === "AbortError" ? "request timed out" : error.message;
        markUnconfirmed(
          `The page cannot currently confirm room state: ${detail}. Agents must treat ambiguous authority as not owned.`,
        );
      }
    };
    read();
    pollingTimer = window.setInterval(read, 2000);
  };

  const observeElection = (room, generation) => {
    if (!isCurrentGeneration(generation)) return;
    if (room.watch_path && "EventSource" in window) {
      const stream = new EventSource(`${apiBase}${room.watch_path}`);
      electionStream = stream;
      stream.addEventListener("state", (event) => {
        if (!isCurrentGeneration(generation) || electionStream !== stream) return;
        try {
          renderElectionState(JSON.parse(event.data), generation, room.election_id, true);
        } catch {
          stream.close();
          if (electionStream === stream) electionStream = undefined;
          markUnconfirmed(
            "The live stream returned an invalid election state. This page no longer confirms authority while it reconciles with the API.",
          );
          if (!pollingTimer) pollElection(room.status_path, room.election_id, generation);
        }
      });
      stream.addEventListener("error", () => {
        if (!isCurrentGeneration(generation) || electionStream !== stream) return;
        stream.close();
        electionStream = undefined;
        if (!pollingTimer) pollElection(room.status_path, room.election_id, generation);
      });
    } else {
      pollElection(room.status_path, room.election_id, generation);
    }
    observationTimer = window.setTimeout(() => endObservation(generation), 15 * 60 * 1000);
  };

  const createSharedRoom = async () => {
    const generation = beginGeneration();
    runButtons.forEach((button) => {
      button.disabled = true;
      button.textContent = "Opening one room…";
    });
    roomLabel.textContent = "creating shared room";
    handoff.hidden = true;
    status.textContent = "The hosted API is creating one room outside both candidates.";
    candidates.forEach((row) => setCandidate(row, "ready", "READY"));

    try {
      const roomResponse = await requestJson(`${apiBase}/elections`, { method: "POST" });
      if (!isCurrentGeneration(generation)) return;
      const room = validateCreateResponse(roomResponse);
      roomLabel.textContent = `room / ${room.election_id}`;
      instructionFields.forEach((field) => {
        field.value = instruction(room.election_id, field.dataset.candidateId);
      });
      handoff.hidden = false;
      candidates.forEach((row) => setCandidate(row, "shared", "ROOM SHARED"));
      status.textContent = "The real room is live and vacant. Copy one instruction to each agent; this page will observe who becomes leader.";
      observeElection(room, generation);
      runButtons.forEach((button) => {
        button.textContent = "Create another shared room";
      });
    } catch (error) {
      if (!isCurrentGeneration(generation)) return;
      candidates.forEach((row) => setCandidate(row, "unavailable", "UNAVAILABLE"));
      roomLabel.textContent = "hosted authority unavailable";
      handoff.hidden = true;
      const detail = error.name === "AbortError" ? "request timed out" : error.message;
      status.textContent = `The live API did not confirm coordination: ${detail}. No winner is shown and no agent should act.`;
      runButtons.forEach((button) => {
        button.textContent = "Try the hosted API again";
      });
    } finally {
      if (!isCurrentGeneration(generation)) return;
      runButtons.forEach((button) => {
        button.disabled = false;
      });
    }
  };

  runButtons.forEach((button) => {
    button.addEventListener("click", async () => {
      await createSharedRoom();
      if (!demo.contains(button)) {
        demo.scrollIntoView({ behavior: "smooth", block: "start" });
      }
    });
  });
  window.addEventListener("pagehide", beginGeneration);
})();
