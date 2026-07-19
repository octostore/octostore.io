(() => {
  const copyButtons = document.querySelectorAll("[data-copy-target]");

  for (const button of copyButtons) {
    button.addEventListener("click", async () => {
      const target = document.querySelector(button.dataset.copyTarget);
      if (!target) return;

      const original = button.textContent;
      try {
        await navigator.clipboard.writeText(target.textContent.trim());
        button.textContent = "Copied";
      } catch {
        button.textContent = "Select to copy";
        const selection = window.getSelection();
        const range = document.createRange();
        range.selectNodeContents(target);
        selection.removeAllRanges();
        selection.addRange(range);
      }

      window.setTimeout(() => {
        button.textContent = original;
      }, 1800);
    });
  }

  const year = document.querySelector("[data-current-year]");
  if (year) year.textContent = new Date().getFullYear().toString();

  const demo = document.querySelector("[data-election-demo]");
  if (!demo) return;

  const runButton = demo.querySelector("[data-run-election]");
  const roomLabel = demo.querySelector("[data-election-room]");
  const status = demo.querySelector("[data-race-status]");
  const candidates = [...demo.querySelectorAll("[data-candidate]")];
  const apiBase = demo.dataset.apiBase || "https://api.octostore.io";

  const resetCandidate = (row) => {
    row.dataset.state = "waiting";
    row.querySelector("[data-candidate-state]").textContent = "READY";
  };

  const runElection = async () => {
    runButton.disabled = true;
    runButton.textContent = "ELECTING…";
    roomLabel.textContent = "opening room…";
    status.textContent = "Three processes are asking the same remote referee for leadership.";
    candidates.forEach(resetCandidate);

    try {
      const roomResponse = await fetch(`${apiBase}/elections`, { method: "POST" });
      if (!roomResponse.ok) throw new Error(`Room creation returned ${roomResponse.status}`);
      const room = await roomResponse.json();
      roomLabel.textContent = `room / ${room.election_id}`;

      const results = await Promise.all(
        candidates.map(async (row) => {
          const candidateId = row.dataset.candidate;
          row.querySelector("[data-candidate-state]").textContent = "REQUESTING";
          const response = await fetch(`${apiBase}${room.campaign_path}`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              candidate_id: candidateId,
              ttl_seconds: 15,
              metadata: `live-demo route=${row.dataset.route}`,
            }),
          });
          if (!response.ok) throw new Error(`Campaign returned ${response.status}`);
          return { row, result: await response.json() };
        }),
      );

      const winner = results.find(({ result }) => result.status === "leader");
      for (const { row, result } of results) {
        row.dataset.state = result.status;
        const stateLabel = row.querySelector("[data-candidate-state]");
        stateLabel.textContent =
          result.status === "leader"
            ? `LEADER · TERM ${result.leader.term}`
            : `HOLD · ${result.retry_after_ms}MS`;
      }

      status.textContent = winner
        ? `${winner.result.leader.candidate_id.toUpperCase()} won term ${winner.result.leader.term}. The other processes received the same leader and retry time. No account was created.`
        : "The room responded, but no leader was elected. Try the race again.";
    } catch (error) {
      candidates.forEach(resetCandidate);
      roomLabel.textContent = "remote room unavailable";
      status.textContent = `The live API could not run this race: ${error.message}. The self-hosted API uses the same endpoints.`;
    } finally {
      runButton.disabled = false;
      runButton.textContent = "RUN ANOTHER ELECTION";
    }
  };

  runButton.addEventListener("click", runElection);
})();
