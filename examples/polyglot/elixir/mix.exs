defmodule Demo.MixProject do
  use Mix.Project

  def project do
    [app: :demo, version: "0.1.0", elixir: "~> 1.14", deps: deps()]
  end

  def application, do: [extra_applications: [:logger]]

  defp deps do
    [
      {:jason, "~> 1.3"},
      {:plug, "~> 1.13"},
      {:ecto, "~> 3.8"},
      {:phoenix, "~> 1.6"}
    ]
  end
end
